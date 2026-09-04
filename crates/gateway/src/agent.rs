use anyhow::{Context, Result};
use reqwest::{Client, StatusCode};
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::collections::HashMap;
use zork_agent_api::{
    ApiErrorBody, ApiErrorCode, CreateSessionRequest, ItemList, MailboxRequest, ProfileDocument,
    SessionSummary, SessionView,
};

pub use zork_agent_api::{AgentProfile, SessionSelection};

#[cfg(test)]
use serde_json::json;

use crate::config::RuntimeConfig;
use crate::db::{GatewayDb, ProactiveBindingRow, SessionBindingRow, SessionRow};

const IM_SYSTEM_PROMPT: &str = include_str!("../prompts/im-thread-base-instructions.md");
const SLACK_PROACTIVE_SYSTEM_PROMPT: &str =
    include_str!("../prompts/slack-proactive-base-instructions.md");

pub fn system_prompt_for_binding(binding: &SessionBindingRow) -> &'static str {
    match binding {
        SessionBindingRow::Normal(_) => IM_SYSTEM_PROMPT,
        SessionBindingRow::Proactive(_) => SLACK_PROACTIVE_SYSTEM_PROMPT,
    }
}

pub type CreatedSession = SessionView;

#[derive(Debug)]
pub struct AgentHttpError {
    pub status: StatusCode,
    pub message: String,
}

impl std::fmt::Display for AgentHttpError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for AgentHttpError {}

pub fn base_url(config: &RuntimeConfig) -> String {
    zork_config::loopback_base_url(&config.agent_bind)
}

pub fn authenticate(
    config: &RuntimeConfig,
    request: reqwest::RequestBuilder,
) -> reqwest::RequestBuilder {
    match &config.agent_token {
        Some(token) => request.bearer_auth(token),
        None => request,
    }
}

pub async fn list_profiles(config: &RuntimeConfig) -> Result<Vec<AgentProfile>> {
    let http = client()?;
    let response = authenticate(config, http.get(format!("{}/profiles", base_url(config))))
        .send()
        .await
        .context("list zork-agent profiles")?;
    let list: ItemList<AgentProfile> = response_json(response, "list zork-agent profiles").await?;
    Ok(list.items)
}

pub async fn profile_list_value(config: &RuntimeConfig) -> Result<Value> {
    profiles_value(config).await
}

pub async fn session_statuses(config: &RuntimeConfig) -> Result<HashMap<String, String>> {
    let response = authenticate(
        config,
        client()?.get(format!("{}/sessions", base_url(config))),
    )
    .send()
    .await
    .context("list zork-agent sessions")?;
    let list: ItemList<SessionSummary> =
        response_json(response, "list zork-agent sessions").await?;
    Ok(list
        .items
        .into_iter()
        .map(|session| (session.session_id, session.status.as_str().to_owned()))
        .collect())
}

pub fn default_selection(profiles: &[AgentProfile]) -> Option<SessionSelection> {
    let (model, thinking) = profiles.iter().find_map(|profile| {
        if !profile.auth_configured {
            return None;
        }
        let model = profile.models.iter().find(|model| model.default)?;
        model
            .thinking
            .iter()
            .any(|thinking| thinking == &model.default_thinking)
            .then(|| (model.id.clone(), model.default_thinking.clone()))
    })?;
    resolve_selection(
        profiles,
        &SessionSelection {
            profile_id: "auto".to_owned(),
            model,
            thinking,
        },
    )
}

pub fn resolve_selection(
    profiles: &[AgentProfile],
    requested: &SessionSelection,
) -> Option<SessionSelection> {
    let compatible = profiles
        .iter()
        .filter(|profile| {
            profile.auth_configured
                && profile.models.iter().any(|model| {
                    model.id == requested.model
                        && model
                            .thinking
                            .iter()
                            .any(|thinking| thinking == &requested.thinking)
                })
        })
        .collect::<Vec<_>>();
    let profile = if requested.profile_id == "auto" {
        recommended_profile(&compatible)?
    } else {
        compatible
            .into_iter()
            .find(|profile| profile.profile_id == requested.profile_id)?
    };
    Some(SessionSelection {
        profile_id: profile.profile_id.clone(),
        model: requested.model.clone(),
        thinking: requested.thinking.clone(),
    })
}

pub async fn ensure_session(
    config: &RuntimeConfig,
    db: &GatewayDb,
    session: &SessionRow,
) -> Result<String> {
    ensure_binding_session(config, db, &SessionBindingRow::Normal(session.clone())).await
}

pub async fn ensure_proactive_session(
    config: &RuntimeConfig,
    db: &GatewayDb,
    binding: &ProactiveBindingRow,
) -> Result<String> {
    ensure_binding_session(config, db, &SessionBindingRow::Proactive(binding.clone())).await
}

pub async fn ensure_binding_session(
    config: &RuntimeConfig,
    db: &GatewayDb,
    binding: &SessionBindingRow,
) -> Result<String> {
    if let Some(session_id) = binding.id() {
        return Ok(session_id.to_owned());
    }
    let profiles = list_profiles(config).await?;
    let selection = match default_selection(&profiles) {
        Some(selection) => selection,
        None => {
            db.set_binding_selection_block(binding, "no_selectable_profiles")?;
            anyhow::bail!("no selectable Agent profiles");
        }
    };
    let system_prompt = system_prompt_for_binding(binding);
    let created = create_session(
        config,
        &selection,
        Some(system_prompt),
        binding.workspace_path(),
    )
    .await?;
    db.set_binding_agent_session(
        binding,
        &created.session_id,
        &created.workspace,
        &selection.profile_id,
        &selection.model,
        &selection.thinking,
    )?;
    Ok(created.session_id)
}

pub async fn create_binding_session(
    config: &RuntimeConfig,
    db: &GatewayDb,
    binding: &SessionBindingRow,
    selection: &SessionSelection,
) -> std::result::Result<CreatedSession, AgentHttpError> {
    if binding.id().is_some() {
        return Err(AgentHttpError {
            status: StatusCode::CONFLICT,
            message: "binding already has an Agent session".to_owned(),
        });
    }
    let created = create_session(
        config,
        selection,
        Some(system_prompt_for_binding(binding)),
        binding.workspace_path(),
    )
    .await?;
    db.set_binding_agent_session(
        binding,
        &created.session_id,
        &created.workspace,
        &created.profile_id,
        &created.model,
        &created.thinking,
    )
    .map_err(|error| AgentHttpError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: format!("persist Agent binding: {error}"),
    })?;
    Ok(created)
}

pub async fn create_session(
    config: &RuntimeConfig,
    selection: &SessionSelection,
    system_prompt: Option<&str>,
    workspace: &str,
) -> std::result::Result<CreatedSession, AgentHttpError> {
    let http = client().map_err(|error| AgentHttpError {
        status: StatusCode::BAD_GATEWAY,
        message: error.to_string(),
    })?;
    let response = authenticate(config, http.post(format!("{}/sessions", base_url(config))))
        .json(&CreateSessionRequest {
            context: None,
            profile_id: selection.profile_id.clone(),
            model: selection.model.clone(),
            thinking: selection.thinking.clone(),
            system_prompt: system_prompt.map(str::to_owned),
            workspace: Some(workspace.to_owned()),
        })
        .send()
        .await
        .map_err(|error| AgentHttpError {
            status: StatusCode::BAD_GATEWAY,
            message: format!("create zork-agent session: {error}"),
        })?;
    let created: CreatedSession =
        response_json_with_status(response, StatusCode::CREATED, "create zork-agent session")
            .await?;
    if created.profile_id != selection.profile_id
        || created.model != selection.model
        || created.thinking != selection.thinking
    {
        return Err(AgentHttpError {
            status: StatusCode::BAD_GATEWAY,
            message: "zork-agent returned a different session selection".to_owned(),
        });
    }
    if created.workspace.is_empty() {
        return Err(AgentHttpError {
            status: StatusCode::BAD_GATEWAY,
            message: "zork-agent returned an empty session workspace".to_owned(),
        });
    }
    Ok(created)
}

pub async fn update_selection(
    config: &RuntimeConfig,
    session_id: &str,
    selection: &SessionSelection,
) -> std::result::Result<SessionSelection, AgentHttpError> {
    // PUT /sessions/{id}/selection —— 事实事件，下一轮生效。
    let http = client().map_err(|error| AgentHttpError {
        status: StatusCode::BAD_GATEWAY,
        message: error.to_string(),
    })?;
    let request = http
        .put(format!(
            "{}/sessions/{session_id}/selection",
            base_url(config)
        ))
        .json(selection);
    let response = authenticate(config, request)
        .send()
        .await
        .map_err(|error| AgentHttpError {
            status: StatusCode::BAD_GATEWAY,
            message: error.to_string(),
        })?;
    let session: SessionView =
        response_json_with_status(response, StatusCode::OK, "update zork-agent selection").await?;
    let updated = session.selection();
    if session.session_id != session_id || &updated != selection {
        return Err(AgentHttpError {
            status: StatusCode::BAD_GATEWAY,
            message: "zork-agent returned a different session selection".to_owned(),
        });
    }
    Ok(updated)
}

/// Context policy lives only in Agent state; the Gateway does not cache or
/// independently persist a second copy.
pub async fn session_context(
    config: &RuntimeConfig,
    session_id: &str,
    update: Option<&zork_agent_api::ContextConfig>,
) -> std::result::Result<zork_agent_api::ContextConfig, AgentHttpError> {
    let http = client().map_err(|error| AgentHttpError {
        status: StatusCode::BAD_GATEWAY,
        message: error.to_string(),
    })?;
    let url = format!("{}/sessions/{session_id}", base_url(config));
    let request = match update {
        Some(context) => http.put(format!("{url}/context")).json(context),
        None => http.get(url),
    };
    let response = authenticate(config, request)
        .send()
        .await
        .map_err(|error| AgentHttpError {
            status: StatusCode::BAD_GATEWAY,
            message: error.to_string(),
        })?;
    let session: SessionView =
        response_json_with_status(response, StatusCode::OK, "session context").await?;
    if session.session_id != session_id
        || update.is_some_and(|expected| expected != &session.context)
    {
        return Err(AgentHttpError {
            status: StatusCode::BAD_GATEWAY,
            message: "zork-agent returned a different session context".into(),
        });
    }
    Ok(session.context)
}

pub async fn append_mailbox(config: &RuntimeConfig, session_id: &str, content: &str) -> Result<()> {
    let http = client()?;
    let response = authenticate(
        config,
        http.post(format!(
            "{}/sessions/{session_id}/mailbox",
            base_url(config)
        )),
    )
    .json(&MailboxRequest {
        content: content.to_owned(),
    })
    .send()
    .await
    .context("append zork-agent mailbox")?;
    response_empty(response, StatusCode::ACCEPTED, "append zork-agent mailbox").await
}

pub async fn cancel_session(config: &RuntimeConfig, session_id: &str) -> Result<bool> {
    let response = authenticate(
        config,
        client()?.post(format!("{}/sessions/{session_id}/cancel", base_url(config))),
    )
    .send()
    .await
    .context("cancel zork-agent session")?;
    let status = response.status();
    match status {
        StatusCode::NO_CONTENT => Ok(true),
        StatusCode::NOT_FOUND => {
            let error = decode_error_response(response, "cancel zork-agent session").await?;
            if error.error.code != ApiErrorCode::SessionNotFound {
                anyhow::bail!(
                    "cancel zork-agent session returned HTTP 404 with error code {:?}",
                    error.error.code
                );
            }
            Ok(false)
        }
        _ => Err(anyhow::anyhow!(
            "zork-agent cancellation failed: {}",
            decode_error_response(response, "cancel zork-agent session")
                .await?
                .error
                .message
        )),
    }
}

pub async fn put_profile(
    config: &RuntimeConfig,
    profile_id: &str,
    document: &Value,
) -> std::result::Result<Value, AgentHttpError> {
    let document: ProfileDocument =
        serde_json::from_value(document.clone()).map_err(|error| AgentHttpError {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            message: format!("invalid zork-agent profile document: {error}"),
        })?;
    let http = client().map_err(|error| AgentHttpError {
        status: StatusCode::BAD_GATEWAY,
        message: error.to_string(),
    })?;
    let response = authenticate(
        config,
        http.put(format!("{}/profiles/{profile_id}", base_url(config))),
    )
    .json(&document)
    .send()
    .await
    .map_err(|error| AgentHttpError {
        status: StatusCode::BAD_GATEWAY,
        message: format!("put zork-agent profile: {error}"),
    })?;
    let profile: AgentProfile =
        response_json_with_status(response, StatusCode::OK, "put zork-agent profile").await?;
    serde_json::to_value(profile).map_err(|error| AgentHttpError {
        status: StatusCode::BAD_GATEWAY,
        message: format!("encode zork-agent profile response: {error}"),
    })
}

pub async fn delete_profile(
    config: &RuntimeConfig,
    profile_id: &str,
) -> std::result::Result<(), AgentHttpError> {
    let http = client().map_err(|error| AgentHttpError {
        status: StatusCode::BAD_GATEWAY,
        message: error.to_string(),
    })?;
    let response = authenticate(
        config,
        http.delete(format!("{}/profiles/{profile_id}", base_url(config))),
    )
    .send()
    .await
    .map_err(|error| AgentHttpError {
        status: StatusCode::BAD_GATEWAY,
        message: format!("delete zork-agent profile: {error}"),
    })?;
    response_empty_with_status(
        response,
        StatusCode::NO_CONTENT,
        "delete zork-agent profile",
    )
    .await
}

pub async fn profiles_value(config: &RuntimeConfig) -> Result<Value> {
    let response = authenticate(
        config,
        client()?.get(format!("{}/profiles", base_url(config))),
    )
    .send()
    .await
    .context("list zork-agent profiles")?;
    let profiles: ItemList<AgentProfile> =
        response_json(response, "list zork-agent profiles").await?;
    serde_json::to_value(profiles).context("encode zork-agent profile list")
}

fn client() -> Result<Client> {
    Client::builder()
        .no_proxy()
        .build()
        .context("Agent HTTP client")
}

async fn response_json<T: DeserializeOwned>(
    response: reqwest::Response,
    operation: &str,
) -> Result<T> {
    if !response.status().is_success() {
        let error = decode_error_response(response, operation).await?;
        anyhow::bail!("{operation} failed: {}", error.error.message);
    }
    response
        .json::<T>()
        .await
        .with_context(|| format!("invalid {operation} response"))
}

async fn response_empty(
    response: reqwest::Response,
    expected: StatusCode,
    operation: &str,
) -> Result<()> {
    if response.status() == expected {
        return Ok(());
    }
    let error = decode_error_response(response, operation).await?;
    anyhow::bail!("{operation} failed: {}", error.error.message)
}

async fn decode_error_response(
    response: reqwest::Response,
    operation: &str,
) -> Result<ApiErrorBody> {
    let status = response.status();
    response
        .json::<ApiErrorBody>()
        .await
        .with_context(|| format!("invalid {operation} error response for HTTP {status}"))
}

async fn response_json_with_status<T: DeserializeOwned>(
    response: reqwest::Response,
    expected: StatusCode,
    operation: &str,
) -> std::result::Result<T, AgentHttpError> {
    if response.status() != expected {
        return Err(agent_http_error(response, operation).await);
    }
    response.json::<T>().await.map_err(|error| AgentHttpError {
        status: StatusCode::BAD_GATEWAY,
        message: format!("invalid {operation} response: {error}"),
    })
}

async fn response_empty_with_status(
    response: reqwest::Response,
    expected: StatusCode,
    operation: &str,
) -> std::result::Result<(), AgentHttpError> {
    if response.status() == expected {
        Ok(())
    } else {
        Err(agent_http_error(response, operation).await)
    }
}

async fn agent_http_error(response: reqwest::Response, operation: &str) -> AgentHttpError {
    let upstream_status = response.status();
    let status = if upstream_status.is_client_error() {
        upstream_status
    } else {
        StatusCode::BAD_GATEWAY
    };
    let message = match response.json::<ApiErrorBody>().await {
        Ok(error) => error.error.message,
        Err(error) => {
            format!("invalid {operation} error response for HTTP {upstream_status}: {error}")
        }
    };
    AgentHttpError { status, message }
}

fn recommended_profile<'a>(profiles: &[&'a AgentProfile]) -> Option<&'a AgentProfile> {
    let scored = profiles
        .iter()
        .map(|profile| (*profile, remaining_score(profile)))
        .filter(|(_, score)| *score > 0.0)
        .collect::<Vec<_>>();
    let mut pool = if scored.is_empty() {
        profiles
            .iter()
            .map(|profile| (*profile, 0.0))
            .collect::<Vec<_>>()
    } else {
        let subscriptions = scored
            .iter()
            .copied()
            .filter(|(profile, _)| profile.billing == "subscription")
            .collect::<Vec<_>>();
        if subscriptions.is_empty() {
            scored
        } else {
            subscriptions
        }
    };
    pool.sort_by(|(left, left_score), (right, right_score)| {
        right_score
            .partial_cmp(left_score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| left.profile_id.cmp(&right.profile_id))
    });
    pool.first().map(|(profile, _)| *profile)
}

fn remaining_score(profile: &AgentProfile) -> f64 {
    if profile.account.get("ok").and_then(Value::as_bool) != Some(true)
        || profile.rate_limits.get("ok").and_then(Value::as_bool) != Some(true)
    {
        return 0.0;
    }
    if profile.billing == "usage" {
        let credits = profile.rate_limits.pointer("/rateLimits/credits");
        if credits.and_then(|value| value.get("unlimited").and_then(Value::as_bool)) == Some(true) {
            return 100.0;
        }
        return credits
            .and_then(|value| value.get("balance"))
            .and_then(|value| {
                value
                    .as_f64()
                    .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
            })
            .unwrap_or(0.0);
    }
    let used = profile
        .rate_limits
        .pointer("/rateLimits/secondary/usedPercent")
        .and_then(Value::as_f64)
        .or_else(|| {
            profile
                .rate_limits
                .pointer("/rateLimits/primary/usedPercent")
                .and_then(Value::as_f64)
        })
        .unwrap_or(100.0);
    (100.0 - used).max(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(
        profile_id: &str,
        billing: &str,
        account: Value,
        rate_limits: Value,
    ) -> AgentProfile {
        serde_json::from_value(json!({
            "profile_id": profile_id,
            "provider": "test",
            "billing": billing,
            "auth_configured": true,
            "account": account,
            "rateLimits": rate_limits,
            "models": [{
                "id": "grok-4.6",
                "api": "openai-responses",
                "streaming": true,
                "parallel_tool_calls": false,
                "thinking": ["high", "xhigh"],
                "default_thinking": "xhigh",
                "capabilities": {"input": ["text"]},
                "default": true
            }]
        }))
        .unwrap()
    }

    #[test]
    fn selects_only_an_explicit_profile_default_model_and_thinking() {
        let profiles = vec![profile("grok", "subscription", json!({}), json!({}))];
        assert_eq!(
            default_selection(&profiles),
            Some(SessionSelection {
                profile_id: "grok".to_owned(),
                model: "grok-4.6".to_owned(),
                thinking: "xhigh".to_owned(),
            })
        );
    }

    #[test]
    fn automatic_profile_keeps_the_requested_model_and_thinking() {
        let profiles = vec![
            profile(
                "usage",
                "usage",
                json!({ "ok": true }),
                json!({ "ok": true, "rateLimits": { "credits": { "balance": "100" } } }),
            ),
            profile(
                "subscription",
                "subscription",
                json!({ "ok": true }),
                json!({ "ok": true, "rateLimits": { "secondary": { "usedPercent": 60 } } }),
            ),
        ];
        assert_eq!(
            resolve_selection(
                &profiles,
                &SessionSelection {
                    profile_id: "auto".to_owned(),
                    model: "grok-4.6".to_owned(),
                    thinking: "high".to_owned(),
                },
            ),
            Some(SessionSelection {
                profile_id: "subscription".to_owned(),
                model: "grok-4.6".to_owned(),
                thinking: "high".to_owned(),
            })
        );
    }
}
