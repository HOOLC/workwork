use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::Utc;
use reqwest::Client;
use serde_json::{json, Value};

use super::{
    nonempty, urlencoding, AuthProvider, BillingKind, DeviceCode, DeviceCodePoll, ProfileTemplate,
    ProviderInfo, QuotaSnapshot,
};

// VS Code GitHub Copilot OAuth app (base64 "SXYxLmI1MDdhMDhjODdlY2ZlOTg=").
const CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";
const DEVICE_CODE_URL: &str = "https://github.com/login/device/code";
const ACCESS_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";
const COPILOT_TOKEN_URL: &str = "https://api.github.com/copilot_internal/v2/token";
const API_BASE: &str = "https://api.individual.githubcopilot.com";
const SCOPE: &str = "read:user";
const USER_AGENT: &str = "GitHubCopilotChat/0.35.0";
const EDITOR_VERSION: &str = "vscode/1.107.0";
const EDITOR_PLUGIN_VERSION: &str = "copilot-chat/0.35.0";
const COPILOT_INTEGRATION_ID: &str = "vscode-chat";
const REFRESH_LEEWAY_MS: i64 = 5 * 60 * 1000;

pub struct GithubCopilot;

fn copilot_headers() -> Value {
    json!({
        "User-Agent": USER_AGENT,
        "Editor-Version": EDITOR_VERSION,
        "Editor-Plugin-Version": EDITOR_PLUGIN_VERSION,
        "Copilot-Integration-Id": COPILOT_INTEGRATION_ID,
    })
}

fn default_models() -> Value {
    json!([{
        "id": "gpt-4.1",
        "api": "openai-completions",
        "streaming": true,
        "thinking": ["off"],
        "default_thinking": "off",
        "capabilities": { "input": ["text"] },
        "limits": {
            "context_window_tokens": 1_047_576,
            "max_output_tokens": 32_768
        },
        "default": true
    }])
}

fn with_copilot_headers(request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    request
        .header("User-Agent", USER_AGENT)
        .header("Editor-Version", EDITOR_VERSION)
        .header("Editor-Plugin-Version", EDITOR_PLUGIN_VERSION)
        .header("Copilot-Integration-Id", COPILOT_INTEGRATION_ID)
        .header("Accept", "application/json")
}

#[async_trait]
impl AuthProvider for GithubCopilot {
    fn info(&self) -> ProviderInfo {
        ProviderInfo {
            id: "github-copilot",
            label: "GitHub Copilot",
            billing: &[
                BillingKind {
                    id: "subscription",
                    label: "Copilot 订阅",
                },
                BillingKind {
                    id: "usage",
                    label: "API 按量",
                },
            ],
        }
    }

    fn template(&self, billing: &str) -> Result<ProfileTemplate> {
        match billing {
            "subscription" | "usage" => Ok(ProfileTemplate {
                provider: "github-copilot".into(),
                billing: billing.into(),
                base_url: API_BASE.into(),
                headers: copilot_headers(),
                models: default_models(),
            }),
            other => anyhow::bail!("github-copilot does not support billing {other}"),
        }
    }

    fn decorate_document(&self, document: &mut Value) {
        let access = document
            .pointer("/auth/access")
            .and_then(Value::as_str)
            .or_else(|| document.pointer("/auth/key").and_then(Value::as_str));
        if let Some(base_url) = access.and_then(base_url_from_token) {
            if let Some(object) = document.as_object_mut() {
                object.insert("base_url".into(), json!(base_url));
            }
        }
    }

    fn bearer(&self, auth: &Value) -> Result<String> {
        nonempty(auth.get("access"))
            .or_else(|| nonempty(auth.get("key")))
            .context("Missing GitHub Copilot credential")
    }

    async fn probe(&self, http: &Client, document: &Value) -> Result<QuotaSnapshot> {
        let billing = document
            .get("billing")
            .and_then(Value::as_str)
            .unwrap_or("subscription");
        let mut auth = document.get("auth").cloned().unwrap_or(json!({}));
        let previous = auth.clone();
        if billing == "subscription" || nonempty(auth.get("refresh")).is_some() {
            auth = self.refresh_if_needed(http, auth).await?;
        }
        probe_models(http, document, &auth, billing, previous != auth).await
    }

    async fn refresh_auth(&self, http: &Client, auth: Value) -> Result<Value> {
        refresh_copilot_auth(http, auth).await
    }

    async fn refresh_if_needed(&self, http: &Client, auth: Value) -> Result<Value> {
        if nonempty(auth.get("refresh")).is_none() {
            return Ok(auth);
        }
        let Some(expires) = expires_ms(&auth) else {
            return Ok(auth);
        };
        let now = Utc::now().timestamp_millis();
        if expires > now + REFRESH_LEEWAY_MS {
            return Ok(auth);
        }
        refresh_copilot_auth(http, auth).await
    }

    fn supports_device_code(&self, billing: &str) -> bool {
        billing == "subscription"
    }

    async fn start_device_code(&self, http: &Client) -> Result<DeviceCode> {
        let body = format!(
            "client_id={}&scope={}",
            urlencoding(CLIENT_ID),
            urlencoding(SCOPE)
        );
        let response = http
            .post(DEVICE_CODE_URL)
            .header("Accept", "application/json")
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("User-Agent", USER_AGENT)
            .body(body)
            .timeout(Duration::from_secs(20))
            .send()
            .await
            .context("github copilot device code")?;
        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("GitHub Copilot device code failed: {body}");
        }
        let payload: Value = response.json().await.context("device code json")?;
        let device_code = payload
            .get("device_code")
            .and_then(Value::as_str)
            .context("missing device_code")?;
        let user_code = payload
            .get("user_code")
            .and_then(Value::as_str)
            .context("missing user_code")?;
        let verification_url = payload
            .get("verification_uri_complete")
            .and_then(Value::as_str)
            .or_else(|| payload.get("verification_uri").and_then(Value::as_str))
            .or_else(|| payload.get("verification_url").and_then(Value::as_str))
            .unwrap_or("https://github.com/login/device");
        let interval = match &payload["interval"] {
            Value::Number(number) => number.as_u64().unwrap_or(5),
            Value::String(text) => text.trim().parse().unwrap_or(5),
            _ => 5,
        };
        let expires_in = payload
            .get("expires_in")
            .and_then(Value::as_i64)
            .unwrap_or(900);
        let expires_at = Utc::now() + chrono::Duration::seconds(expires_in);
        Ok(DeviceCode {
            provider: "github-copilot".into(),
            billing: "subscription".into(),
            device_code: device_code.to_string(),
            user_code: user_code.to_string(),
            verification_url: verification_url.to_string(),
            interval_seconds: interval.max(1),
            expires_at: expires_at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            extra: json!({}),
        })
    }

    async fn poll_device_code(
        &self,
        http: &Client,
        pending: &DeviceCode,
    ) -> Result<DeviceCodePoll> {
        let body = format!(
            "client_id={}&device_code={}&grant_type={}",
            urlencoding(CLIENT_ID),
            urlencoding(&pending.device_code),
            urlencoding("urn:ietf:params:oauth:grant-type:device_code")
        );
        let response = http
            .post(ACCESS_TOKEN_URL)
            .header("Accept", "application/json")
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("User-Agent", USER_AGENT)
            .body(body)
            .timeout(Duration::from_secs(20))
            .send()
            .await
            .context("github copilot device poll")?;
        let payload: Value = response.json().await.unwrap_or(json!({}));
        if let Some(github_token) = nonempty(payload.get("access_token")) {
            let auth = mint_copilot_auth(http, github_token).await?;
            return Ok(DeviceCodePoll::Completed { auth });
        }
        let error = payload.get("error").and_then(Value::as_str).unwrap_or("");
        match error {
            "authorization_pending" => Ok(DeviceCodePoll::Pending {
                retry_after_seconds: pending.interval_seconds,
            }),
            "slow_down" => Ok(DeviceCodePoll::Pending {
                retry_after_seconds: payload
                    .get("interval")
                    .and_then(Value::as_u64)
                    .unwrap_or(pending.interval_seconds.max(5) + 5),
            }),
            "" => anyhow::bail!("GitHub Copilot device poll failed"),
            other => anyhow::bail!("GitHub Copilot device poll failed: {other}"),
        }
    }
}

async fn mint_copilot_auth(http: &Client, github_token: String) -> Result<Value> {
    let (access, expires) = fetch_copilot_token(http, &github_token).await?;
    Ok(json!({
        "type": "oauth",
        "access": access,
        "refresh": github_token,
        "expires": expires,
    }))
}

async fn refresh_copilot_auth(http: &Client, auth: Value) -> Result<Value> {
    let refresh = nonempty(auth.get("refresh")).context("Missing GitHub Copilot refresh token")?;
    let (access, expires) = fetch_copilot_token(http, &refresh).await?;
    let mut next = auth;
    if let Some(map) = next.as_object_mut() {
        map.insert("type".into(), json!("oauth"));
        map.insert("access".into(), json!(access));
        map.insert("refresh".into(), json!(refresh));
        map.insert("expires".into(), json!(expires));
    }
    Ok(next)
}

async fn fetch_copilot_token(http: &Client, github_token: &str) -> Result<(String, i64)> {
    let response = with_copilot_headers(http.get(COPILOT_TOKEN_URL))
        .header("Authorization", format!("Bearer {github_token}"))
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .context("github copilot token")?;
    if !response.status().is_success() {
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("GitHub Copilot token request failed: {body}");
    }
    let payload: Value = response.json().await.context("github copilot token json")?;
    let token = payload
        .get("token")
        .and_then(Value::as_str)
        .context("missing copilot token")?;
    let expires_at = json_i64(payload.get("expires_at"))
        .or_else(|| claim_i64(token, "exp"))
        .context("missing copilot expires_at")?;
    Ok((token.to_string(), unix_to_ms(expires_at)))
}

async fn probe_models(
    http: &Client,
    document: &Value,
    auth: &Value,
    billing: &str,
    auth_changed: bool,
) -> Result<QuotaSnapshot> {
    let bearer = GithubCopilot.bearer(auth)?;
    let base_url = nonempty(document.get("base_url"))
        .or_else(|| base_url_from_token(&bearer))
        .unwrap_or_else(|| API_BASE.to_string())
        .trim_end_matches('/')
        .to_string();
    let response = with_copilot_headers(http.get(format!("{base_url}/models")))
        .header("Authorization", format!("Bearer {bearer}"))
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .context("github copilot models")?;
    if !response.status().is_success() {
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("GitHub Copilot models probe failed: {body}");
    }
    let (plan_type, credits, limit_id, limit_name) = if billing == "usage" {
        (
            "api",
            json!({ "unlimited": true, "balance": null }),
            "github_copilot_usage",
            "GitHub Copilot API",
        )
    } else {
        (
            "subscription",
            Value::Null,
            "github_copilot",
            "GitHub Copilot",
        )
    };
    Ok(QuotaSnapshot {
        account: json!({
            "ok": true,
            "account": {
                "type": "github-copilot",
                "planType": if billing == "usage" { "API" } else { "Copilot" }
            },
            "requiresOpenaiAuth": false
        }),
        rate_limits: json!({
            "ok": true,
            "rateLimits": {
                "limitId": limit_id,
                "limitName": limit_name,
                "primary": null,
                "secondary": null,
                "credits": credits,
                "planType": plan_type
            },
            "rateLimitsByLimitId": {}
        }),
        auth: if auth_changed {
            Some(auth.clone())
        } else {
            None
        },
    })
}

fn base_url_from_token(token: &str) -> Option<String> {
    let host = claim(token, "proxy-ep")?;
    let host = host.trim();
    if host.is_empty() {
        return None;
    }
    let api_host = host
        .strip_prefix("proxy.")
        .map(|rest| format!("api.{rest}"))
        .unwrap_or_else(|| host.to_string());
    Some(format!("https://{api_host}"))
}

fn claim(token: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}=");
    token.split(';').find_map(|part| {
        part.trim()
            .strip_prefix(&prefix)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
    })
}

fn claim_i64(token: &str, key: &str) -> Option<i64> {
    claim(token, key)?.parse().ok()
}

fn json_i64(value: Option<&Value>) -> Option<i64> {
    let value = value?;
    value
        .as_i64()
        .or_else(|| value.as_u64().and_then(|number| i64::try_from(number).ok()))
        .or_else(|| value.as_f64().map(|number| number as i64))
        .or_else(|| value.as_str()?.trim().parse().ok())
}

fn unix_to_ms(value: i64) -> i64 {
    if value > 1_000_000_000_000 {
        value
    } else {
        value.saturating_mul(1000)
    }
}

fn expires_ms(auth: &Value) -> Option<i64> {
    json_i64(auth.get("expires")).map(unix_to_ms)
}
