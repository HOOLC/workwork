use anyhow::{Context, Result};
use reqwest::StatusCode;
use serde_json::{json, Value};
use tracing::info;

use crate::config::RuntimeConfig;
use crate::connections::ConnectionRuntime;
use crate::db::{EnsureSession, GatewayDb, SessionRow};
use crate::inbound::InboundEvent;
use crate::jobs::JobEvent;
use crate::slack::BotSelf;
use crate::state::AppState;

pub async fn handle_event(
    state: &AppState,
    connection: &ConnectionRuntime,
    event: &InboundEvent,
) -> Result<()> {
    if let Some(self_json) = &event.self_json {
        apply_bot(connection, self_json).await;
    }
    handle_inbound(state, connection, event.clone()).await
}

async fn apply_bot(connection: &ConnectionRuntime, self_json: &Value) {
    let user_id = self_json
        .get("userId")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if user_id.is_empty() {
        return;
    }
    let mention = self_json
        .get("mention")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| format!("<@{user_id}>"));
    *connection.bot.lock().await = Some(BotSelf {
        user_id,
        mention,
        raw: self_json.clone(),
    });
}

pub async fn handle_inbound(
    state: &AppState,
    connection: &ConnectionRuntime,
    event: InboundEvent,
) -> Result<()> {
    if connection.config.mode == zork_config::ImMode::Proactive {
        return handle_proactive_inbound(state, connection, event).await;
    }
    let session_key = session_key(&connection.config.id, &event);
    info!(
        connection_id = %connection.config.id,
        session = %session_key,
        source = %event.source,
        "chat.message.accepted"
    );
    let _ = state.db.insert_admin_event(
        "inbound",
        "session",
        Some(&session_key),
        event.message_id.as_deref(),
        &json!({
            "connectionId": connection.config.id,
            "provider": connection.config.provider_name(),
            "source": event.source,
            "conversationId": event.conversation_id,
            "rootMessageId": event.root_message_id,
        }),
    );

    let creates_session = matches!(event.source.as_str(), "app_mention" | "direct_message");
    let existing = state.db.get_session(&session_key)?;
    if !creates_session && existing.is_none() {
        return Ok(());
    }
    let session = state.db.ensure_session(EnsureSession {
        connection_id: &connection.config.id,
        platform: connection.config.provider_name(),
        channel_id: &event.conversation_id,
        root_thread_ts: &event.root_message_id,
        channel_type: event.channel_type.as_deref(),
        initiator_user_id: (event.sender_kind == "user").then_some(event.sender_user_id.as_str()),
        initiator_message_ts: event.message_id.as_deref(),
    })?;
    if let Some((name, channel_type)) = connection
        .slack
        .conversation_info(&event.conversation_id)
        .await
    {
        state
            .db
            .set_channel_metadata(&session.key, name.as_deref(), channel_type.as_deref())?;
    }
    let session = state
        .db
        .get_session(&session.key)?
        .context("session missing")?;

    if event.is_stop() {
        let stopped = match session.id.as_deref() {
            Some(session_id) => crate::agent::cancel_session(&state.config, session_id).await?,
            None => false,
        };
        connection
            .status
            .clear_thread(&session.channel_id, &session.root_thread_ts)
            .await;
        connection
            .slack
            .post_thread_message(
                &session.channel_id,
                &session.root_thread_ts,
                if stopped {
                    "Stopped the current run."
                } else {
                    "No active run to stop."
                },
            )
            .await
            .ok();
        state.db.touch_reply(&session.key)?;
        return Ok(());
    }
    if event.is_empty() {
        return Ok(());
    }

    let message_id = event
        .message_id
        .as_deref()
        .context("IM message is missing messageId")?;
    if state
        .db
        .inbound_status(&session.key, message_id)?
        .as_deref()
        == Some("delivered")
    {
        return Ok(());
    }
    let history = if existing.is_none()
        && event.source == "app_mention"
        && event.message_id.as_deref() != Some(event.root_message_id.as_str())
    {
        load_history_text(state, connection, &event).await
    } else {
        None
    };
    let content = format_event(connection, &event, history.as_deref()).await;
    let delivery = append_to_agent_with_projection(state, session.clone(), &content).await;
    state.db.record_inbound(
        &session.key,
        &connection.config.id,
        &session.channel_id,
        &session.root_thread_ts,
        message_id,
        &event.source,
        &event.sender_user_id,
        &event.text,
        event.channel_type.as_deref(),
        if delivery.is_ok() {
            "delivered"
        } else {
            "blocked"
        },
    )?;
    delivery
}

async fn handle_proactive_inbound(
    state: &AppState,
    connection: &ConnectionRuntime,
    event: InboundEvent,
) -> Result<()> {
    if event.is_empty() {
        return Ok(());
    }
    let message_id = event
        .message_id
        .as_deref()
        .context("IM message is missing messageId")?;
    let inbound_key = format!(
        "{}:{}:{message_id}",
        connection.config.id, event.conversation_id
    );
    info!(
        connection_id = %connection.config.id,
        channel = %event.conversation_id,
        message_id,
        source = %event.source,
        "chat.message.proactive.accepted"
    );
    if state.db.proactive_inbound_status(&inbound_key)?.as_deref() == Some("delivered") {
        return Ok(());
    }

    let binding = state
        .db
        .ensure_proactive_binding(&connection.config.id, connection.config.provider_name())?;
    let _ = state.db.insert_admin_event(
        "inbound",
        "proactive",
        Some(&binding.key),
        event.message_id.as_deref(),
        &json!({
            "connectionId": connection.config.id,
            "provider": connection.config.provider_name(),
            "source": event.source,
            "conversationId": event.conversation_id,
            "rootMessageId": event.root_message_id,
        }),
    );
    let content = format_proactive_event(connection, &event).await;
    let delivery = async {
        let agent_id =
            crate::agent::ensure_proactive_session(&state.config, &state.db, &binding).await?;
        crate::agent::append_mailbox(&state.config, &agent_id, &content).await
    }
    .await;
    state.db.record_proactive_inbound(
        &inbound_key,
        &binding.key,
        &connection.config.id,
        &event.conversation_id,
        event.channel_type.as_deref(),
        &event.root_message_id,
        message_id,
        &event.source,
        &event.sender_user_id,
        &event.text,
        if delivery.is_ok() {
            "delivered"
        } else {
            "blocked"
        },
    )?;
    delivery
}

pub async fn handle_job_event(
    config: &RuntimeConfig,
    db: &GatewayDb,
    event: JobEvent,
) -> Result<()> {
    let binding = db
        .get_binding(&event.session_key)?
        .context("session_not_found")?;
    let content = format!(
        "A broker-managed background job reported a new asynchronous event for this session.\njob_id: {}\njob_kind: {}\nevent_kind: {}\nsummary: {}",
        event.job_id, event.kind, event.event_kind, event.summary
    );
    let agent_id = crate::agent::ensure_binding_session(config, db, &binding).await?;
    crate::agent::append_mailbox(config, &agent_id, &content).await
}

async fn append_to_agent_with_projection(
    state: &AppState,
    session: SessionRow,
    content: &str,
) -> Result<()> {
    let agent_id = crate::agent::ensure_session(&state.config, &state.db, &session).await?;
    state
        .status_projection
        .ensure(
            &session.key,
            &agent_id,
            &session.connection_id,
            &session.channel_id,
            &session.root_thread_ts,
        )
        .await;
    crate::agent::append_mailbox(&state.config, &agent_id, content).await
}

pub async fn reset_session(state: &AppState, session_key: &str) -> Result<bool> {
    let binding = state
        .db
        .get_binding(session_key)?
        .with_context(|| format!("Unknown session: {session_key}"))?;
    if let Some(session_id) = binding.id() {
        delete_agent_session(state, session_id).await?;
    }
    state.status_projection.remove(session_key).await;
    state.db.clear_binding_agent_session(&binding)?;

    let current = state
        .db
        .get_binding(session_key)?
        .with_context(|| format!("Unknown session: {session_key}"))?;
    let instruction = "A session reset was requested by an administrator. Treat earlier Agent history as cleared, rebuild context from the current IM thread, and continue only work that is still relevant.";
    match current {
        crate::db::SessionBindingRow::Normal(session) => {
            let connection = state
                .connections
                .runtime(&session.connection_id)
                .await
                .context("session connection is not configured")?;
            let history = load_thread_history_text(
                state,
                &connection,
                &session.channel_id,
                &session.root_thread_ts,
                None,
            )
            .await;
            let content = match history {
                Some(history) => format!("{history}\n\n{instruction}"),
                None => instruction.to_owned(),
            };
            append_to_agent_with_projection(state, session, &content).await?;
        }
        crate::db::SessionBindingRow::Proactive(binding) => {
            let binding = crate::db::SessionBindingRow::Proactive(binding);
            let agent_id =
                crate::agent::ensure_binding_session(&state.config, &state.db, &binding).await?;
            crate::agent::append_mailbox(&state.config, &agent_id, instruction).await?;
        }
    }
    Ok(true)
}

pub async fn delete_session(state: &AppState, session_key: &str) -> Result<bool> {
    let binding = state
        .db
        .get_binding(session_key)?
        .with_context(|| format!("Unknown session: {session_key}"))?;
    if let Some(session_id) = binding.id() {
        delete_agent_session(state, session_id).await?;
    }
    let deleted = state.db.delete_binding(&binding)?;
    if deleted {
        state.status_projection.remove(session_key).await;
    }
    Ok(deleted)
}

async fn delete_agent_session(state: &AppState, agent_id: &str) -> Result<()> {
    let request = reqwest::Client::builder()
        .no_proxy()
        .build()?
        .delete(format!(
            "{}/sessions/{agent_id}",
            crate::agent::base_url(&state.config)
        ));
    let response = crate::agent::authenticate(&state.config, request)
        .send()
        .await
        .context("delete canonical Agent session")?;
    let status = response.status();
    if status != StatusCode::NO_CONTENT && status != StatusCode::NOT_FOUND {
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("Agent session deletion failed ({status}): {body}");
    }
    Ok(())
}

async fn format_event(
    connection: &ConnectionRuntime,
    event: &InboundEvent,
    earlier_thread_context: Option<&str>,
) -> String {
    let sender = if event.sender_kind == "user" {
        connection.slack.user_identity(&event.sender_user_id).await
    } else {
        None
    };
    let payload = json!({
        "origin": {
            "connection_id": connection.config.id,
            "connection_name": connection.config.name,
            "provider": connection.config.provider_name(),
        },
        "conversation_id": event.conversation_id,
        "root_message_id": event.root_message_id,
        "source": event.source,
        "message_ts": event.message_id,
        "sender": {
            "user_id": event.sender_user_id,
            "kind": event.sender_kind,
            "display_name": sender.as_ref().and_then(|value| value.get("displayName").cloned()),
        },
        "mentioned_user_ids": event.mentioned_user_ids,
        "text": if event.text.trim().is_empty() { "[no text body]".into() } else { event.text.clone() },
        "attachments": event.attachments,
    });
    let current = format!(
        "A new message arrived in the IM thread. Carefully judge whether it requires a reply or action from you.\nstructured_message_json:\n```json\n{}\n```",
        serde_json::to_string_pretty(&payload).unwrap_or_else(|_| payload.to_string())
    );
    match earlier_thread_context.filter(|value| !value.trim().is_empty()) {
        Some(context) => format!("{context}\n\nCurrent message requiring attention:\n{current}"),
        None => current,
    }
}

async fn format_proactive_event(connection: &ConnectionRuntime, event: &InboundEvent) -> String {
    let sender = if event.sender_kind == "user" {
        connection.slack.user_identity(&event.sender_user_id).await
    } else {
        None
    };
    let payload = json!({
        "origin": {
            "connection_id": connection.config.id,
            "connection_name": connection.config.name,
            "provider": connection.config.provider_name(),
        },
        "channel_id": event.conversation_id,
        "thread_ts": event.root_message_id,
        "message_ts": event.message_id,
        "channel_type": event.channel_type,
        "source": event.source,
        "sender": {
            "user_id": event.sender_user_id,
            "kind": event.sender_kind,
            "display_name": sender.as_ref().and_then(|value| value.get("displayName").cloned()),
        },
        "mentioned_user_ids": event.mentioned_user_ids,
        "text": if event.text.trim().is_empty() { "[no text body]".into() } else { event.text.clone() },
        "attachments": event.attachments,
        "provider_message": event.slack_message,
    });
    format!(
        "An IM message was observed by the proactive Gateway. It is context for triage, not automatically a request addressed to you. Decide whether to remain silent or provide useful help according to the proactive system instructions.\nobserved_message_json:\n```json\n{}\n```",
        serde_json::to_string_pretty(&payload).unwrap_or_else(|_| payload.to_string())
    )
}

async fn load_history_text(
    state: &AppState,
    connection: &ConnectionRuntime,
    event: &InboundEvent,
) -> Option<String> {
    load_thread_history_text(
        state,
        connection,
        &event.conversation_id,
        &event.root_message_id,
        event.message_id.as_deref(),
    )
    .await
}

async fn load_thread_history_text(
    state: &AppState,
    connection: &ConnectionRuntime,
    conversation_id: &str,
    root_message_id: &str,
    before_message_id: Option<&str>,
) -> Option<String> {
    let history = connection
        .slack
        .thread_history(
            conversation_id,
            root_message_id,
            before_message_id,
            Some(state.config.slack_initial_thread_history_count),
        )
        .await
        .ok()?;
    let messages = history.get("messages").and_then(Value::as_array)?;
    if messages.is_empty() {
        return None;
    }
    Some(format!(
        "Earlier IM thread context before the current message. Treat these history items as context only; do not reply to them individually.\nhistory_count: {}\n{}",
        messages.len(),
        serde_json::to_string_pretty(messages).unwrap_or_default()
    ))
}

pub async fn post_message(
    state: &AppState,
    session_key: &str,
    conversation_id: &str,
    root_message_id: &str,
    text: &str,
    kind: Option<&str>,
) -> Result<()> {
    state
        .entries
        .post_message(session_key, conversation_id, root_message_id, text, kind)
        .await
}

fn session_key(connection_id: &str, event: &InboundEvent) -> String {
    format!(
        "{connection_id}:{}:{}",
        event.conversation_id, event.root_message_id
    )
}
