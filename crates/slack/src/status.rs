use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use reqwest::Client;
use serde_json::Value;
use tokio::sync::Mutex;
use tracing::{info, warn};

const EYES: &str = "eyes";

/// Supplies the current bot token and API base; zork wires this to its
/// config file so token rotation takes effect without a restart.
pub type TokenProvider = Arc<dyn Fn() -> Option<(String, String)> + Send + Sync>;

#[derive(Clone)]
pub struct AssistantStatusHub {
    http: Client,
    slack_bot_token: String,
    slack_api_base_url: String,
    tokens_provider: TokenProvider,
    threads: Arc<Mutex<HashMap<String, ThreadState>>>,
}

#[derive(Default)]
struct ThreadState {
    last_status: String,
    pending: Option<String>,
    fallback_only: bool,
    flushing: bool,
}

impl AssistantStatusHub {
    pub fn new(
        http: Client,
        slack_bot_token: impl Into<String>,
        slack_api_base_url: impl Into<String>,
    ) -> Self {
        Self::with_token_provider(http, slack_bot_token, slack_api_base_url, Arc::new(|| None))
    }

    pub fn with_token_provider(
        http: Client,
        slack_bot_token: impl Into<String>,
        slack_api_base_url: impl Into<String>,
        tokens_provider: TokenProvider,
    ) -> Self {
        Self {
            http,
            slack_bot_token: slack_bot_token.into(),
            slack_api_base_url: slack_api_base_url.into(),
            tokens_provider,
            threads: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Refreshes tokens from the current config; callers that never rotate
    /// tokens can ignore this.
    fn refresh_tokens(&mut self) {
        if let Some((token, base)) = (self.tokens_provider)() {
            self.slack_bot_token = token;
            self.slack_api_base_url = base;
        }
    }

    pub async fn clear_thread(&self, channel_id: &str, thread_ts: &str) {
        self.set_thread(channel_id, thread_ts, "").await;
    }

    pub async fn set_thread(&self, channel_id: &str, thread_ts: &str, status: &str) {
        if channel_id.is_empty() || thread_ts.is_empty() {
            return;
        }
        let key = format!("{channel_id}:{thread_ts}");
        let mut map = self.threads.lock().await;
        let state = map.entry(key.clone()).or_default();
        let shown = state.pending.as_deref().unwrap_or(&state.last_status);
        if shown == status {
            return;
        }
        state.pending = Some(status.to_string());
        if state.flushing {
            return;
        }
        state.flushing = true;
        drop(map);
        let mut hub = self.clone();
        let channel_id = channel_id.to_string();
        let thread_ts = thread_ts.to_string();
        hub.refresh_tokens();
        tokio::spawn(async move {
            hub.flush(&key, &channel_id, &thread_ts).await;
        });
    }

    async fn flush(&mut self, key: &str, channel_id: &str, thread_ts: &str) {
        loop {
            let next = {
                let mut map = self.threads.lock().await;
                let Some(state) = map.get_mut(key) else {
                    return;
                };
                loop {
                    match state.pending.take() {
                        Some(status) if status != state.last_status => break Some(status),
                        Some(_) => {}
                        None => {
                            state.flushing = false;
                            break None;
                        }
                    }
                }
            };
            let Some(status) = next else {
                return;
            };
            info!(channel_id, thread_ts, status = %status, "slack assistant status");
            let fallback_only = {
                let map = self.threads.lock().await;
                map.get(key)
                    .map(|state| state.fallback_only)
                    .unwrap_or(false)
            };
            let result = deliver(
                &self.http,
                &self.slack_bot_token,
                &self.slack_api_base_url,
                channel_id,
                thread_ts,
                &status,
                fallback_only,
            )
            .await;
            {
                let mut map = self.threads.lock().await;
                let state = map.entry(key.to_string()).or_default();
                match result {
                    Delivered::Status => {
                        state.fallback_only = false;
                        state.last_status = status;
                    }
                    Delivered::Reaction => {
                        state.fallback_only = true;
                        state.last_status = status;
                    }
                    Delivered::Failed => {}
                }
            }
        }
    }
}

enum Delivered {
    Status,
    Reaction,
    Failed,
}

async fn deliver(
    http: &Client,
    bot_token: &str,
    api_base_url: &str,
    channel_id: &str,
    thread_ts: &str,
    status: &str,
    fallback_only: bool,
) -> Delivered {
    if !fallback_only {
        match slack_form(
            http,
            bot_token,
            api_base_url,
            "assistant.threads.setStatus",
            &[
                ("channel_id", channel_id),
                ("thread_ts", thread_ts),
                ("status", status),
            ],
        )
        .await
        {
            Ok(()) => {
                let _ = slack_form(
                    http,
                    bot_token,
                    api_base_url,
                    "reactions.remove",
                    &[
                        ("channel", channel_id),
                        ("timestamp", thread_ts),
                        ("name", EYES),
                    ],
                )
                .await;
                return Delivered::Status;
            }
            Err(error) if should_fallback(&error.to_string()) => {}
            Err(error) => {
                warn!(
                    error = %error,
                    channel_id,
                    thread_ts,
                    status,
                    "failed to update Slack assistant thread status"
                );
                return Delivered::Failed;
            }
        }
    }
    let method = if status.trim().is_empty() {
        "reactions.remove"
    } else {
        "reactions.add"
    };
    match slack_form(
        http,
        bot_token,
        api_base_url,
        method,
        &[
            ("channel", channel_id),
            ("timestamp", thread_ts),
            ("name", EYES),
        ],
    )
    .await
    {
        Ok(()) => Delivered::Reaction,
        Err(error) => {
            let message = error.to_string();
            if message.contains("already_reacted") || message.contains("no_reaction") {
                return Delivered::Reaction;
            }
            warn!(
                error = %error,
                channel_id,
                thread_ts,
                "failed to update Slack assistant fallback reaction"
            );
            Delivered::Failed
        }
    }
}

async fn slack_form(
    http: &Client,
    bot_token: &str,
    api_base_url: &str,
    method: &str,
    fields: &[(&str, &str)],
) -> Result<()> {
    let body = fields
        .iter()
        .map(|(key, value)| format!("{}={}", urlencode(key), urlencode(value)))
        .collect::<Vec<_>>()
        .join("&");
    let response = http
        .post(format!("{api_base_url}/{method}"))
        .header("authorization", format!("Bearer {bot_token}"))
        .header(
            "content-type",
            "application/x-www-form-urlencoded; charset=utf-8",
        )
        .body(body)
        .send()
        .await
        .with_context(|| format!("slack {method}"))?;
    let payload: Value = response
        .json()
        .await
        .with_context(|| format!("slack {method} json"))?;
    if payload.get("ok") != Some(&Value::Bool(true)) {
        let error = payload
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("unknown_error");
        anyhow::bail!("Slack API error for {method}: {error}");
    }
    Ok(())
}

fn should_fallback(error: &str) -> bool {
    [
        "missing_scope",
        "unknown_method",
        "not_allowed",
        "not_enabled",
        "is_bot",
        "unsupported",
        "feature_not_enabled",
        "invalid_arguments",
    ]
    .iter()
    .any(|needle| error.contains(needle))
}

fn urlencode(value: &str) -> String {
    let mut out = String::new();
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char);
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}
