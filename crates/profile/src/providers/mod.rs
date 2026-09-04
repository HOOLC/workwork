mod anthropic;
mod github_copilot;
mod kimi;
mod openai;
mod openai_compatible;
mod opencode_go;
mod openrouter;
mod xai;

use anyhow::{Context, Result};
use async_trait::async_trait;
use reqwest::Client;
use serde_json::{json, Value};

pub use anthropic::Anthropic;
pub use github_copilot::GithubCopilot;
pub use kimi::Kimi;
pub use openai::OpenAi;
pub use openai_compatible::OpenAiCompatible;
pub use opencode_go::OpenCodeGo;
pub use openrouter::OpenRouter;
pub use xai::Xai;

/// One billing mode a provider ships a profile template for.
#[derive(Clone, Copy, Debug)]
pub struct BillingKind {
    pub id: &'static str,
    pub label: &'static str,
}

/// Catalog entry shown to callers (admin UI, SDK).
#[derive(Clone, Copy, Debug)]
pub struct ProviderInfo {
    pub id: &'static str,
    pub label: &'static str,
    pub billing: &'static [BillingKind],
}

/// Empty profile document for one billing mode, including aimux base_url / headers / models.
#[derive(Clone, Debug)]
pub struct ProfileTemplate {
    pub provider: String,
    pub billing: String,
    pub base_url: String,
    pub headers: Value,
    pub models: Value,
}

impl ProfileTemplate {
    pub fn document(&self, auth: Value) -> Value {
        json!({
            "provider": self.provider,
            "billing": self.billing,
            "base_url": self.base_url,
            "headers": self.headers,
            "models": self.models,
            "auth": auth,
        })
    }
}

#[derive(Clone, Debug)]
pub struct QuotaSnapshot {
    pub account: Value,
    pub rate_limits: Value,
    pub auth: Option<Value>,
}

pub(crate) fn with_refreshed_auth(
    mut snapshot: QuotaSnapshot,
    previous: &Value,
    refreshed: Value,
) -> QuotaSnapshot {
    if previous != &refreshed {
        snapshot.auth = Some(refreshed);
    }
    snapshot
}

#[derive(Clone, Debug)]
pub struct DeviceCode {
    pub provider: String,
    pub billing: String,
    pub device_code: String,
    pub user_code: String,
    pub verification_url: String,
    pub interval_seconds: u64,
    pub expires_at: String,
    pub extra: Value,
}

impl DeviceCode {
    pub fn public_id(&self) -> String {
        format!("{}:{}", self.provider, self.device_code)
    }

    pub fn to_start_json(&self) -> Value {
        json!({
            "deviceCode": {
                "deviceAuthId": self.public_id(),
                "userCode": self.user_code,
                "verificationUrl": self.verification_url,
                "intervalSeconds": self.interval_seconds,
                "expiresAt": self.expires_at,
                "provider": self.provider,
                "billing": self.billing,
            }
        })
    }
}

impl Default for DeviceCode {
    fn default() -> Self {
        Self {
            provider: String::new(),
            billing: "subscription".into(),
            device_code: String::new(),
            user_code: String::new(),
            verification_url: String::new(),
            interval_seconds: 5,
            expires_at: String::new(),
            extra: json!({}),
        }
    }
}

pub enum DeviceCodePoll {
    Pending { retry_after_seconds: u64 },
    Completed { auth: Value },
}

/// Per-provider auth, quota, and login. Model HTTP stays in aimux.
#[async_trait]
pub trait AuthProvider: Send + Sync {
    fn info(&self) -> ProviderInfo;
    fn template(&self, billing: &str) -> Result<ProfileTemplate>;
    fn decorate_document(&self, _document: &mut Value) {}
    fn decorate_execution_headers(
        &self,
        _billing: &str,
        _model: &str,
        _headers: &mut std::collections::HashMap<String, String>,
    ) {
    }
    fn bearer(&self, auth: &Value) -> Result<String>;
    async fn probe(&self, http: &Client, document: &Value) -> Result<QuotaSnapshot>;
    async fn refresh_auth(&self, http: &Client, auth: Value) -> Result<Value>;
    async fn refresh_if_needed(&self, http: &Client, auth: Value) -> Result<Value>;
    fn supports_device_code(&self, billing: &str) -> bool;
    async fn start_device_code(&self, http: &Client) -> Result<DeviceCode>;
    async fn poll_device_code(&self, http: &Client, pending: &DeviceCode)
        -> Result<DeviceCodePoll>;
}

static XAI: Xai = Xai;
static OPENAI: OpenAi = OpenAi;
static OPENAI_COMPATIBLE: OpenAiCompatible = OpenAiCompatible;
static OPENCODE_GO: OpenCodeGo = OpenCodeGo;
static ANTHROPIC: Anthropic = Anthropic;
static GITHUB_COPILOT: GithubCopilot = GithubCopilot;
static OPENROUTER: OpenRouter = OpenRouter;
static KIMI: Kimi = Kimi;

pub fn all() -> [&'static dyn AuthProvider; 8] {
    [
        &XAI,
        &OPENAI,
        &OPENAI_COMPATIBLE,
        &OPENCODE_GO,
        &ANTHROPIC,
        &GITHUB_COPILOT,
        &OPENROUTER,
        &KIMI,
    ]
}

pub fn get(id: &str) -> Result<&'static dyn AuthProvider> {
    all()
        .into_iter()
        .find(|provider| provider.info().id == id)
        .context(format!("unknown provider {id}"))
}

pub fn catalog() -> Value {
    json!({
        "providers": all()
            .iter()
            .map(|provider| {
                let info = provider.info();
                json!({
                    "id": info.id,
                    "label": info.label,
                    "billing": info
                        .billing
                        .iter()
                        .map(|kind| {
                            json!({
                                "id": kind.id,
                                "label": kind.label,
                                "deviceCode": provider.supports_device_code(kind.id),
                                "template": provider.template(kind.id).ok().map(|template| json!({
                                    "baseUrl": template.base_url,
                                    "models": template.models,
                                })),
                            })
                        })
                        .collect::<Vec<_>>(),
                })
            })
            .collect::<Vec<_>>(),
    })
}

pub fn parse_device_auth_id(value: &str) -> Result<(&str, &str)> {
    value
        .split_once(':')
        .filter(|(provider, code)| !provider.is_empty() && !code.is_empty())
        .context("deviceAuthId must be provider:device_code")
}

pub(crate) fn nonempty(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

pub(crate) fn urlencoding(value: &str) -> String {
    let mut out = String::new();
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

pub(crate) fn has_credential(auth: &Value) -> bool {
    match auth.get("type").and_then(Value::as_str) {
        Some("oauth") => {
            nonempty(auth.get("access")).is_some() && nonempty(auth.get("refresh")).is_some()
        }
        Some("api_key") => nonempty(auth.get("key")).is_some(),
        _ => nonempty(auth.get("access"))
            .or_else(|| nonempty(auth.get("key")))
            .is_some(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_snapshot_returns_auth_only_when_refresh_changed_it() {
        let snapshot = QuotaSnapshot {
            account: json!({ "ok": true }),
            rate_limits: json!({ "ok": true }),
            auth: None,
        };
        let original = json!({ "access": "old" });

        assert!(
            with_refreshed_auth(snapshot.clone(), &original, original.clone())
                .auth
                .is_none()
        );
        assert_eq!(
            with_refreshed_auth(snapshot, &original, json!({ "access": "new" })).auth,
            Some(json!({ "access": "new" }))
        );
    }
}
