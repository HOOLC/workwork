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

const CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
const TOKEN_URL: &str = "https://auth.x.ai/oauth2/token";
const DEVICE_CODE_URL: &str = "https://auth.x.ai/oauth2/device/code";
const SCOPE: &str = "openid profile email offline_access grok-cli:access api:access";
const BILLING_URL: &str = "https://cli-chat-proxy.grok.com/v1/billing?format=credits";
const SETTINGS_URL: &str = "https://cli-chat-proxy.grok.com/v1/settings";
const MANAGEMENT_API_ROOT: &str = "https://management-api.x.ai/v1";
const SUBSCRIPTION_BASE: &str = "https://cli-chat-proxy.grok.com/v1";
const API_BASE: &str = "https://api.x.ai/v1";
const GROK_CLI_COMPAT_VERSION: &str = "1.0.5";
const REFRESH_LEEWAY_MS: i64 = 5 * 60 * 1000;
const WEEKLY_WINDOW_MINS: i64 = 10_080;

pub struct Xai;

fn grok_headers() -> Value {
    json!({
        "user-agent": "xai-grok-cli",
        "x-xai-token-auth": "xai-grok-cli",
        "x-grok-client-version": GROK_CLI_COMPAT_VERSION,
        "x-grok-client-identifier": "grok-shell",
        "x-grok-client-mode": "headless",
    })
}

fn default_models() -> Value {
    json!([{
        "id": "grok-4.6",
        "api": "openai-completions",
        "streaming": true,
        "thinking": ["low", "medium", "high", "xhigh"],
        "default_thinking": "xhigh",
        "capabilities": { "input": ["text", "image"] },
        "limits": {
            "context_window_tokens": 500_000,
            "max_output_tokens": 8_192
        },
        "default": true
    }])
}

#[async_trait]
impl AuthProvider for Xai {
    fn info(&self) -> ProviderInfo {
        ProviderInfo {
            id: "xai",
            label: "xAI / Grok",
            billing: &[
                BillingKind {
                    id: "subscription",
                    label: "Grok 订阅",
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
            "subscription" => Ok(ProfileTemplate {
                provider: "xai".into(),
                billing: "subscription".into(),
                base_url: SUBSCRIPTION_BASE.into(),
                headers: grok_headers(),
                models: default_models(),
            }),
            "usage" => Ok(ProfileTemplate {
                provider: "xai".into(),
                billing: "usage".into(),
                base_url: API_BASE.into(),
                headers: json!({}),
                models: default_models(),
            }),
            other => anyhow::bail!("xai does not support billing {other}"),
        }
    }

    fn bearer(&self, auth: &Value) -> Result<String> {
        nonempty(auth.get("access"))
            .or_else(|| nonempty(auth.get("key")))
            .context("Missing xAI access token")
    }

    fn decorate_execution_headers(
        &self,
        billing: &str,
        model: &str,
        headers: &mut std::collections::HashMap<String, String>,
    ) {
        if billing == "subscription" {
            headers.insert("x-grok-model-override".into(), model.to_owned());
        }
    }

    async fn probe(&self, http: &Client, document: &Value) -> Result<QuotaSnapshot> {
        let billing = document
            .get("billing")
            .and_then(Value::as_str)
            .unwrap_or("subscription");
        let mut auth = document.get("auth").cloned().unwrap_or(json!({}));
        let (snapshot, next_auth) = if billing == "usage" {
            inspect_prepaid(http, &auth).await?
        } else {
            inspect_subscription(http, &auth).await?
        };
        let changed = next_auth != auth;
        auth = next_auth;
        Ok(QuotaSnapshot {
            account: snapshot.get("account").cloned().unwrap_or(json!({})),
            rate_limits: snapshot.get("rateLimits").cloned().unwrap_or(json!({})),
            auth: if changed { Some(auth) } else { None },
        })
    }

    async fn refresh_auth(&self, http: &Client, auth: Value) -> Result<Value> {
        refresh_xai_auth(http, auth).await
    }

    async fn refresh_if_needed(&self, http: &Client, auth: Value) -> Result<Value> {
        if refresh_token_of(&auth).is_none() {
            return Ok(auth);
        }
        let Some(expires) = expires_ms(&auth) else {
            return Ok(auth);
        };
        let now = Utc::now().timestamp_millis();
        if expires > now + REFRESH_LEEWAY_MS {
            return Ok(auth);
        }
        refresh_xai_auth(http, auth).await
    }

    fn supports_device_code(&self, billing: &str) -> bool {
        billing == "subscription"
    }

    async fn start_device_code(&self, http: &Client) -> Result<DeviceCode> {
        let body = format!(
            "client_id={}&scope={}&referrer=zork",
            urlencoding(CLIENT_ID),
            urlencoding(SCOPE)
        );
        let response = http
            .post(DEVICE_CODE_URL)
            .header("content-type", "application/x-www-form-urlencoded")
            .body(body)
            .timeout(Duration::from_secs(20))
            .send()
            .await
            .context("xAI device code")?;
        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("xAI device code failed: {body}");
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
            .unwrap_or("https://accounts.x.ai/connect/device");
        let interval = payload.get("interval").and_then(Value::as_u64).unwrap_or(5);
        let expires_in = payload
            .get("expires_in")
            .and_then(Value::as_i64)
            .unwrap_or(900);
        let expires_at = Utc::now() + chrono::Duration::seconds(expires_in);
        Ok(DeviceCode {
            provider: "xai".into(),
            billing: "subscription".into(),
            device_code: device_code.to_string(),
            user_code: user_code.to_string(),
            verification_url: verification_url.to_string(),
            interval_seconds: interval,
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
            "grant_type={}&device_code={}&client_id={}",
            urlencoding("urn:ietf:params:oauth:grant-type:device_code"),
            urlencoding(&pending.device_code),
            urlencoding(CLIENT_ID)
        );
        let response = http
            .post(TOKEN_URL)
            .header("content-type", "application/x-www-form-urlencoded")
            .body(body)
            .timeout(Duration::from_secs(20))
            .send()
            .await
            .context("xAI device poll")?;
        let status = response.status();
        let payload: Value = response.json().await.unwrap_or(json!({}));
        if status.is_success() {
            let access = payload
                .get("access_token")
                .and_then(Value::as_str)
                .context("missing access_token")?;
            let refresh = payload
                .get("refresh_token")
                .and_then(Value::as_str)
                .unwrap_or("");
            let expires_in = payload
                .get("expires_in")
                .and_then(Value::as_i64)
                .unwrap_or(3600);
            let expires_at = Utc::now().timestamp_millis() + expires_in * 1000;
            return Ok(DeviceCodePoll::Completed {
                auth: json!({
                    "type": "oauth",
                    "access": access,
                    "refresh": refresh,
                    "expires": expires_at,
                }),
            });
        }
        let error = payload.get("error").and_then(Value::as_str).unwrap_or("");
        match error {
            "authorization_pending" => Ok(DeviceCodePoll::Pending {
                retry_after_seconds: pending.interval_seconds,
            }),
            "slow_down" => Ok(DeviceCodePoll::Pending {
                retry_after_seconds: pending.interval_seconds.max(5) + 5,
            }),
            other => anyhow::bail!("xAI device poll failed: {other}"),
        }
    }
}

async fn inspect_subscription(http: &Client, auth: &Value) -> Result<(Value, Value)> {
    let mut current = Xai.refresh_if_needed(http, auth.clone()).await?;
    let mut response = fetch_xai_json(http, BILLING_URL, Xai.bearer(&current)?).await?;
    if response.status().as_u16() == 401 && refresh_token_of(&current).is_some() {
        current = refresh_xai_auth(http, current).await?;
        response = fetch_xai_json(http, BILLING_URL, Xai.bearer(&current)?).await?;
    }
    if !response.status().is_success() {
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("xAI billing API failed: {body}");
    }
    let billing: Value = response.json().await.context("xAI billing json")?;
    let config = billing.get("config").cloned().unwrap_or(billing.clone());
    let used_percent = credit_usage_percent(&config).context("missing creditUsagePercent")?;
    let period = config.get("currentPeriod").cloned().unwrap_or(json!({}));
    let period_end =
        timestamp_ms(period.get("end")).or_else(|| timestamp_ms(config.get("billingPeriodEnd")));
    let weekly = json!({
        "usedPercent": used_percent,
        "windowDurationMins": WEEKLY_WINDOW_MINS,
        "resetsAt": period_end.map(|ms| ms / 1000),
    });
    let settings = match fetch_xai_json(http, SETTINGS_URL, Xai.bearer(&current)?).await {
        Ok(response) if response.status().is_success() => response.json::<Value>().await.ok(),
        _ => None,
    };
    let email = current
        .get("email")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .or_else(|| {
            settings.as_ref().and_then(|value| {
                value
                    .get("email")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
            })
        });
    let plan = settings
        .as_ref()
        .and_then(|value| {
            value
                .get("subscription_tier_display")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        })
        .or_else(|| {
            current
                .get("auth_mode")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        })
        .unwrap_or_else(|| "xai".to_string());
    let limits = json!({
        "limitId": "xai",
        "limitName": "xAI",
        "primary": null,
        "secondary": weekly,
        "credits": null,
        "planType": null
    });
    if let Some(email) = email.as_ref() {
        if let Some(object) = current.as_object_mut() {
            object.insert("email".into(), json!(email));
        }
    }
    Ok((
        json!({
            "account": {
                "ok": true,
                "account": { "email": email, "type": "xai", "planType": plan },
                "requiresOpenaiAuth": false
            },
            "rateLimits": {
                "ok": true,
                "rateLimits": limits,
                "rateLimitsByLimitId": { "xai": limits }
            }
        }),
        current,
    ))
}

async fn inspect_prepaid(http: &Client, auth: &Value) -> Result<(Value, Value)> {
    let key = auth
        .get("managementKey")
        .and_then(Value::as_str)
        .or_else(|| {
            auth.get("env")
                .and_then(|env| env.get("XAI_MANAGEMENT_API_KEY").and_then(Value::as_str))
        })
        .or_else(|| {
            if auth.get("type").and_then(Value::as_str) == Some("api_key") {
                auth.get("key").and_then(Value::as_str)
            } else {
                None
            }
        })
        .context("xAI usage profile requires a Management API key")?;
    let team_id = auth
        .get("teamId")
        .and_then(Value::as_str)
        .or_else(|| auth.get("team_id").and_then(Value::as_str))
        .or_else(|| {
            auth.get("env")
                .and_then(|env| env.get("XAI_TEAM_ID").and_then(Value::as_str))
        })
        .context("xAI usage profile requires teamId")?;
    let url = format!(
        "{MANAGEMENT_API_ROOT}/billing/teams/{}/prepaid/balance",
        urlencoding(team_id)
    );
    let response = http
        .get(url)
        .header("authorization", format!("Bearer {key}"))
        .header("accept", "application/json")
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .context("xAI prepaid")?;
    if !response.status().is_success() {
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("xAI prepaid balance API failed: {body}");
    }
    let payload: Value = response.json().await.context("prepaid json")?;
    let cents = payload
        .get("total")
        .and_then(Value::as_f64)
        .context("prepaid total")?;
    let dollars = (-cents / 100.0).max(0.0);
    Ok((
        json!({
            "account": {
                "ok": true,
                "account": {
                    "email": auth.get("email"),
                    "type": "xai",
                    "planType": "API"
                },
                "requiresOpenaiAuth": false
            },
            "rateLimits": {
                "ok": true,
                "rateLimits": {
                    "limitId": "xai_usage",
                    "limitName": "xAI usage",
                    "primary": null,
                    "secondary": null,
                    "credits": {
                        "hasCredits": dollars > 0.0,
                        "unlimited": false,
                        "balance": format!("{dollars:.2}")
                    },
                    "planType": "api"
                },
                "rateLimitsByLimitId": {}
            }
        }),
        auth.clone(),
    ))
}

async fn refresh_xai_auth(http: &Client, auth: Value) -> Result<Value> {
    let refresh = refresh_token_of(&auth).context("Missing xAI refresh token")?;
    let body = format!(
        "grant_type=refresh_token&client_id={}&refresh_token={}",
        urlencoding(CLIENT_ID),
        urlencoding(&refresh)
    );
    let response = http
        .post(TOKEN_URL)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(body)
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .context("xAI refresh")?;
    if !response.status().is_success() {
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("xAI token refresh failed: {body}");
    }
    let payload: Value = response.json().await.context("refresh json")?;
    let access = payload
        .get("access_token")
        .and_then(Value::as_str)
        .context("missing access_token")?;
    let next_refresh = payload
        .get("refresh_token")
        .and_then(Value::as_str)
        .unwrap_or(&refresh);
    let expires_in = payload
        .get("expires_in")
        .and_then(Value::as_i64)
        .unwrap_or(3600);
    let expires_at = Utc::now().timestamp_millis() + expires_in * 1000;
    let mut next = auth;
    if let Some(map) = next.as_object_mut() {
        map.insert("type".into(), json!("oauth"));
        map.insert("access".into(), json!(access));
        map.insert("refresh".into(), json!(next_refresh));
        map.insert("expires".into(), json!(expires_at));
    }
    Ok(next)
}

async fn fetch_xai_json(http: &Client, url: &str, bearer: String) -> Result<reqwest::Response> {
    http.get(url)
        .header("authorization", format!("Bearer {bearer}"))
        .header("accept", "application/json")
        .header("user-agent", "xai-grok-cli")
        .header("x-xai-token-auth", "xai-grok-cli")
        .header("x-grok-client-version", GROK_CLI_COMPAT_VERSION)
        .header("x-grok-client-identifier", "grok-shell")
        .header("x-grok-client-mode", "headless")
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .context("xAI request")
}

fn refresh_token_of(auth: &Value) -> Option<String> {
    nonempty(auth.get("refresh")).or_else(|| nonempty(auth.get("refresh_token")))
}

fn expires_ms(auth: &Value) -> Option<i64> {
    if let Some(expires) = auth.get("expires").and_then(Value::as_i64) {
        return Some(if expires > 1_000_000_000_000 {
            expires
        } else {
            expires * 1000
        });
    }
    timestamp_ms(auth.get("expires_at"))
}

fn credit_usage_percent(config: &Value) -> Option<f64> {
    if let Some(direct) = config.get("creditUsagePercent").and_then(Value::as_f64) {
        return Some(direct);
    }
    let cap = money_val(config.get("onDemandCap"));
    let used = money_val(config.get("onDemandUsed"));
    if let (Some(cap), Some(used)) = (cap, used) {
        if cap > 0.0 {
            return Some((used / cap) * 100.0);
        }
    }
    if config.get("currentPeriod").is_some() {
        return Some(0.0);
    }
    None
}

fn money_val(value: Option<&Value>) -> Option<f64> {
    let value = value?;
    if let Some(number) = value.as_f64() {
        return Some(number);
    }
    value.get("val").and_then(Value::as_f64)
}

fn timestamp_ms(value: Option<&Value>) -> Option<i64> {
    let value = value?;
    if let Some(number) = value.as_i64() {
        return Some(if number > 1_000_000_000_000 {
            number
        } else {
            number * 1000
        });
    }
    let text = value.as_str()?;
    chrono::DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|value| value.timestamp_millis())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscription_template_identifies_a_current_grok_cli_and_selected_model() {
        let template = Xai.template("subscription").unwrap();
        let headers = template.headers.as_object().unwrap();
        let selected_model = template.models[0]["id"].as_str().unwrap();

        assert_eq!(headers["x-xai-token-auth"], "xai-grok-cli");
        assert_eq!(headers["x-grok-client-version"], "1.0.5");
        assert_eq!(headers["x-grok-client-identifier"], "grok-shell");
        assert_eq!(headers["x-grok-client-mode"], "headless");

        let mut execution_headers = std::collections::HashMap::from([(
            "x-grok-model-override".to_owned(),
            "stale-model".to_owned(),
        )]);
        Xai.decorate_execution_headers("subscription", selected_model, &mut execution_headers);
        assert_eq!(execution_headers["x-grok-model-override"], selected_model);
    }
}
