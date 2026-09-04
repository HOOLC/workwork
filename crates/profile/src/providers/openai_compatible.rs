use anyhow::{Context, Result};
use async_trait::async_trait;
use reqwest::Client;
use serde_json::{json, Value};

use super::{
    nonempty, AuthProvider, BillingKind, DeviceCode, DeviceCodePoll, ProfileTemplate, ProviderInfo,
    QuotaSnapshot,
};

pub struct OpenAiCompatible;

#[async_trait]
impl AuthProvider for OpenAiCompatible {
    fn info(&self) -> ProviderInfo {
        ProviderInfo {
            id: "openai-compatible",
            label: "OpenAI Compatible",
            billing: &[BillingKind {
                id: "usage",
                label: "Custom API",
            }],
        }
    }

    fn template(&self, billing: &str) -> Result<ProfileTemplate> {
        match billing {
            "usage" => Ok(ProfileTemplate {
                provider: "openai-compatible".into(),
                billing: "usage".into(),
                base_url: "https://example.invalid/v1".into(),
                headers: json!({}),
                models: json!([{
                    "id": "model-id",
                    "api": "openai-responses",
                    "streaming": true,
                    "parallel_tool_calls": false,
                    "thinking": ["off"],
                    "default_thinking": "off",
                    "capabilities": { "input": ["text", "image"] },
                    "default": true
                }]),
            }),
            other => anyhow::bail!("openai-compatible does not support billing {other}"),
        }
    }

    fn bearer(&self, auth: &Value) -> Result<String> {
        nonempty(auth.get("key"))
            .or_else(|| nonempty(auth.get("access")))
            .context("Missing OpenAI-compatible credential")
    }

    async fn probe(&self, _http: &Client, document: &Value) -> Result<QuotaSnapshot> {
        let auth = document.get("auth").cloned().unwrap_or_else(|| json!({}));
        self.bearer(&auth)?;
        Ok(configured_snapshot())
    }

    async fn refresh_auth(&self, _http: &Client, auth: Value) -> Result<Value> {
        Ok(auth)
    }

    async fn refresh_if_needed(&self, _http: &Client, auth: Value) -> Result<Value> {
        Ok(auth)
    }

    fn supports_device_code(&self, _billing: &str) -> bool {
        false
    }

    async fn start_device_code(&self, _http: &Client) -> Result<DeviceCode> {
        anyhow::bail!("openai-compatible uses an API key configured on the profile")
    }

    async fn poll_device_code(
        &self,
        _http: &Client,
        _pending: &DeviceCode,
    ) -> Result<DeviceCodePoll> {
        anyhow::bail!("openai-compatible does not use device-code login")
    }
}

fn configured_snapshot() -> QuotaSnapshot {
    QuotaSnapshot {
        account: json!({
            "ok": true,
            "account": {
                "type": "openai-compatible",
                "planType": "Custom API"
            },
            "requiresOpenaiAuth": false
        }),
        rate_limits: json!({
            "ok": true,
            "reported": false
        }),
        auth: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_status_represents_unreported_quota_without_failure() {
        let snapshot = configured_snapshot();

        assert_eq!(snapshot.account["ok"], true);
        assert_eq!(snapshot.account["account"]["type"], "openai-compatible");
        assert_eq!(
            snapshot.rate_limits,
            json!({ "ok": true, "reported": false })
        );
        assert!(snapshot.auth.is_none());
    }
}
