use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::task::JoinHandle;
use tokio::time::{self, MissedTickBehavior};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{info, warn};

use crate::connections::ConnectionRuntime;
use crate::delivery;
use crate::state::AppState;
use zork_slack::{parse_socket_payload_for_mode, BotIdentity, SlackMessageMode};

struct RunningConnection {
    config: zork_config::ImConnectionConfig,
    task: JoinHandle<()>,
}

pub async fn run_connections(state: AppState, mut shutdown: tokio::sync::watch::Receiver<bool>) {
    let mut revision = state.connections.subscribe();
    let mut running = HashMap::<String, RunningConnection>::new();
    let mut config_check = time::interval(Duration::from_secs(1));
    config_check.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        reconcile(&state, &shutdown, &mut running).await;
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    for (_, connection) in running.drain() {
                        connection.task.abort();
                    }
                    return;
                }
            }
            changed = revision.changed() => {
                if changed.is_err() {
                    return;
                }
            }
            _ = config_check.tick() => {
                if let Err(error) = state.connections.reload_from_disk().await {
                    warn!(error = %error, "IM connection config reload failed");
                }
            }
        }
    }
}

async fn reconcile(
    state: &AppState,
    shutdown: &tokio::sync::watch::Receiver<bool>,
    running: &mut HashMap<String, RunningConnection>,
) {
    let desired = state
        .connections
        .configs()
        .await
        .into_iter()
        .map(|config| (config.id.clone(), config))
        .collect::<HashMap<_, _>>();

    let stopped = running
        .iter()
        .filter_map(|(id, active)| {
            let keep = desired.get(id).is_some_and(|config| {
                config.enabled
                    && config.configured()
                    && *config == active.config
                    && !active.task.is_finished()
            });
            (!keep).then(|| id.clone())
        })
        .collect::<Vec<_>>();
    for id in stopped {
        if let Some(active) = running.remove(&id) {
            active.task.abort();
        }
    }

    for (id, config) in desired {
        if running.contains_key(&id) || !config.enabled || !config.configured() {
            continue;
        }
        let Some(runtime) = state.connections.runtime(&id).await else {
            continue;
        };
        let task_state = state.clone();
        let task_shutdown = shutdown.clone();
        let task = tokio::spawn(async move {
            run_connection(task_state, runtime, task_shutdown).await;
        });
        running.insert(id, RunningConnection { config, task });
    }
}

async fn run_connection(
    state: AppState,
    runtime: Arc<ConnectionRuntime>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let connection_id = runtime.config.id.clone();
    loop {
        if *shutdown.borrow() {
            return;
        }
        state.connections.set_connecting(&connection_id).await;
        match connect_once(&state, &runtime, &mut shutdown).await {
            Ok(()) if *shutdown.borrow() => return,
            Ok(()) => {
                state
                    .connections
                    .set_error(&connection_id, "connection_closed")
                    .await;
            }
            Err(error) => {
                let detail = format!("{error:#}");
                state
                    .connections
                    .set_error(&connection_id, detail.clone())
                    .await;
                warn!(connection_id, error = %detail, "IM connection failed");
            }
        }
        tokio::select! {
            _ = time::sleep(Duration::from_secs(1)) => {}
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return;
                }
            }
        }
    }
}

async fn connect_once(
    state: &AppState,
    runtime: &ConnectionRuntime,
    shutdown: &mut tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    let slack = runtime
        .config
        .slack()
        .context("connection provider is not Slack")?;
    let bot = fetch_bot_identity(slack).await?;
    let bot_self = crate::slack::BotSelf {
        user_id: bot.user_id.clone(),
        mention: format!("<@{}>", bot.user_id),
        raw: json!({
            "surface": bot.surface,
            "userId": bot.user_id,
            "mention": format!("<@{}>", bot.user_id),
            "botId": bot.bot_id,
            "appId": bot.app_id,
            "username": bot.username,
        }),
    };
    *runtime.bot.lock().await = Some(bot_self.clone());
    let url = open_connection(slack).await?;
    let (stream, _) = connect_async(&url)
        .await
        .context("slack websocket connect")?;
    let (mut write, mut read) = stream.split();
    state
        .connections
        .set_connected(&runtime.config.id, bot_self.raw)
        .await;
    info!(connection_id = %runtime.config.id, name = %runtime.config.name, "IM connection connected");

    let mut heartbeat = time::interval(Duration::from_secs(30));
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut awaiting_pong = false;

    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return Ok(());
                }
            }
            _ = heartbeat.tick() => {
                if awaiting_pong {
                    anyhow::bail!("slack websocket heartbeat timed out");
                }
                awaiting_pong = true;
                write.send(Message::Ping(Vec::new().into())).await.context("slack ping")?;
            }
            message = read.next() => {
                let Some(message) = message else {
                    anyhow::bail!("slack websocket closed");
                };
                match message.context("slack websocket read")? {
                    Message::Pong(_) => awaiting_pong = false,
                    Message::Ping(payload) => {
                        write.send(Message::Pong(payload)).await.context("slack pong")?;
                    }
                    Message::Close(_) => anyhow::bail!("slack websocket closed"),
                    Message::Text(text) => {
                        let envelope: SlackEnvelope = serde_json::from_str(&text).context("slack envelope json")?;
                        handle_envelope(state, runtime, &envelope, &bot).await?;
                        if envelope.kind == "disconnect" {
                            anyhow::bail!("slack requested disconnect");
                        }
                        if let Some(envelope_id) = envelope.envelope_id.as_deref() {
                            write.send(Message::Text(json!({ "envelope_id": envelope_id }).to_string().into())).await.context("slack ack")?;
                        }
                    }
                    Message::Binary(_) | Message::Frame(_) => {}
                }
            }
        }
    }
}

#[derive(Debug, Deserialize)]
struct SlackEnvelope {
    #[serde(default)]
    envelope_id: Option<String>,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    payload: Option<Value>,
}

async fn handle_envelope(
    state: &AppState,
    runtime: &ConnectionRuntime,
    envelope: &SlackEnvelope,
    bot: &BotIdentity,
) -> Result<()> {
    if envelope.kind == "hello" || envelope.kind == "disconnect" {
        return Ok(());
    }
    let Some(payload) = envelope.payload.as_ref() else {
        return Ok(());
    };
    let message_mode = match runtime.config.mode {
        zork_config::ImMode::Normal => SlackMessageMode::Thread,
        zork_config::ImMode::Proactive => SlackMessageMode::Proactive,
    };
    let Some((_event_id, inbound)) =
        parse_socket_payload_for_mode(&envelope.kind, payload, bot, message_mode)
    else {
        return Ok(());
    };
    let Some(event) = crate::inbound::parse_inbound_value(&inbound) else {
        return Ok(());
    };
    delivery::handle_event(state, runtime, &event).await
}

#[derive(Debug, Deserialize)]
struct SlackApiResponse {
    ok: bool,
    error: Option<String>,
    url: Option<String>,
    user_id: Option<String>,
    user: Option<String>,
    bot_id: Option<String>,
    app_id: Option<String>,
}

async fn open_connection(config: &zork_config::SlackProviderConfig) -> Result<String> {
    let http = reqwest::Client::builder().no_proxy().build()?;
    let url = format!("{}/apps.connections.open", config.api_base_url());
    let response = http
        .post(&url)
        .header("authorization", format!("Bearer {}", config.app_token))
        .header(
            "content-type",
            "application/x-www-form-urlencoded; charset=utf-8",
        )
        .send()
        .await
        .context("apps.connections.open")?;
    let payload: SlackApiResponse = response
        .json()
        .await
        .context("apps.connections.open json")?;
    if !payload.ok {
        anyhow::bail!(
            "Slack API error for apps.connections.open: {}",
            payload.error.unwrap_or_else(|| "unknown_error".into())
        );
    }
    payload.url.context("apps.connections.open missing url")
}

async fn fetch_bot_identity(config: &zork_config::SlackProviderConfig) -> Result<BotIdentity> {
    let http = reqwest::Client::builder().no_proxy().build()?;
    let url = format!("{}/auth.test", config.api_base_url());
    let response = http
        .post(&url)
        .header("authorization", format!("Bearer {}", config.bot_token))
        .header(
            "content-type",
            "application/x-www-form-urlencoded; charset=utf-8",
        )
        .send()
        .await
        .context("auth.test")?;
    let payload: SlackApiResponse = response.json().await.context("auth.test json")?;
    if !payload.ok {
        anyhow::bail!(
            "Slack API error for auth.test: {}",
            payload.error.unwrap_or_else(|| "unknown_error".into())
        );
    }
    let user_id = payload
        .user_id
        .filter(|value| !value.trim().is_empty())
        .context("auth.test missing user_id")?;
    Ok(BotIdentity {
        user_id,
        bot_id: payload.bot_id.filter(|value| !value.trim().is_empty()),
        app_id: payload.app_id.filter(|value| !value.trim().is_empty()),
        username: payload.user.filter(|value| !value.trim().is_empty()),
        display_name: None,
        real_name: None,
        surface: "Slack".into(),
    })
}
