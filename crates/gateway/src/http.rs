use std::convert::Infallible;
use std::fs;
use std::path::Path as StdPath;
use std::time::Duration;

use anyhow::Context;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use base64::Engine;
use futures_util::Stream;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::net::TcpListener;

use crate::im_entry::{visible_message_json, LOCAL_GUI_ENTRY_ID, LOCAL_GUI_PLATFORM};
use crate::jobs::JobSupervisor;
use crate::state::AppState;
use crate::{delivery, timeline};

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/readyz", get(readyz))
        .route("/healthz", get(readyz))
        .route("/v1/im/profiles", get(local_im_profiles))
        .route(
            "/v1/im/sessions",
            get(list_local_im_sessions).post(create_local_im_session),
        )
        .route(
            "/v1/im/sessions/{session_id}/context",
            get(get_local_im_context).put(update_local_im_context),
        )
        .route(
            "/v1/im/sessions/{session_id}/selection",
            axum::routing::put(update_local_im_selection),
        )
        .route(
            "/v1/im/sessions/{session_id}/messages",
            get(list_local_im_messages).post(post_local_im_message),
        )
        .route("/v1/im/sessions/{session_id}/events", get(local_im_events))
        .route(
            "/v1/im/sessions/{session_id}/cancel",
            post(cancel_local_im_session),
        )
        .route("/internal/realtime/sessions", get(list_sessions))
        .route("/internal/realtime/snapshot", get(snapshot))
        .route("/internal/realtime/logs", get(logs))
        .route("/internal/realtime/preflight", get(preflight))
        .route("/internal/realtime/events", get(events))
        .route(
            "/internal/realtime/sessions/{session_key}/timeline",
            get(timeline),
        )
        .route(
            "/internal/realtime/sessions/{session_key}/timeline-events/{event_id}",
            get(timeline_event),
        )
        .route("/sessions/{session_key}/reset", post(reset_session))
        .route("/sessions/{session_key}", delete(delete_session))
        .route("/github-token/resolve", post(resolve_github_token))
        .route("/jobs/register", post(register_job))
        .route("/jobs/{job_id}/admin-cancel", post(cancel_job))
        .route("/notify", post(notify))
        .route("/chat/thread-history", get(thread_history))
        .route("/chat/post-message", post(post_message))
        .route("/chat/post-file", post(post_file))
        .route("/cli/context", get(cli_context))
        .route("/integrations/mcp-tools", get(mcp_tools))
        .route("/integrations/mcp-call", post(mcp_call))
        .fallback(fallback)
        .with_state(state)
}

async fn local_im_profiles(State(state): State<AppState>) -> Response {
    match crate::agent::profile_list_value(&state.config).await {
        Ok(value) => Json(value).into_response(),
        Err(error) => fail(StatusCode::BAD_GATEWAY, &error.to_string()),
    }
}

async fn list_local_im_sessions(State(state): State<AppState>) -> Response {
    let statuses = crate::agent::session_statuses(&state.config)
        .await
        .unwrap_or_default();
    match state.db.list_sessions() {
        Ok(sessions) => {
            let items = sessions
                .iter()
                .filter(|session| session.platform == LOCAL_GUI_PLATFORM)
                .filter_map(|session| {
                    let session_id = session.id.as_deref()?;
                    Some(local_im_session_json(
                        session,
                        statuses
                            .get(session_id)
                            .map(String::as_str)
                            .unwrap_or("wait"),
                    ))
                })
                .collect::<Vec<_>>();
            Json(json!({ "items": items })).into_response()
        }
        Err(error) => db_error(error),
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateLocalImSession {
    profile_id: String,
    model: String,
    thinking: String,
    workspace: String,
}

async fn create_local_im_session(
    State(state): State<AppState>,
    Json(request): Json<CreateLocalImSession>,
) -> Response {
    if [
        request.profile_id.as_str(),
        request.model.as_str(),
        request.thinking.as_str(),
        request.workspace.as_str(),
    ]
    .iter()
    .any(|value| value.trim().is_empty())
    {
        return fail(
            StatusCode::BAD_REQUEST,
            "profile_id, model, thinking, and workspace are required",
        );
    }
    let workspace = match fs::canonicalize(request.workspace.trim()) {
        Ok(path) if path.is_dir() => path,
        Ok(_) => return fail(StatusCode::BAD_REQUEST, "workspace_is_not_a_directory"),
        Err(error) => {
            return fail(
                StatusCode::BAD_REQUEST,
                &format!("invalid_workspace: {error}"),
            )
        }
    };
    let conversation_id = format!("conversation-{}", ulid::Ulid::new());
    let session = match state.db.create_session_at_workspace(
        crate::db::EnsureSession {
            connection_id: LOCAL_GUI_ENTRY_ID,
            platform: LOCAL_GUI_PLATFORM,
            channel_id: &conversation_id,
            root_thread_ts: &conversation_id,
            channel_type: Some("desktop"),
            initiator_user_id: Some("local-user"),
            initiator_message_ts: None,
        },
        &workspace,
    ) {
        Ok(session) => session,
        Err(error) => return db_error(error),
    };
    let binding = crate::db::SessionBindingRow::Normal(session.clone());
    let selection = crate::agent::SessionSelection {
        profile_id: request.profile_id,
        model: request.model,
        thinking: request.thinking,
    };
    match crate::agent::create_binding_session(&state.config, &state.db, &binding, &selection).await
    {
        Ok(created) => {
            let session = match state.db.get_session(&session.key) {
                Ok(Some(session)) => session,
                Ok(None) => return fail(StatusCode::INTERNAL_SERVER_ERROR, "binding_disappeared"),
                Err(error) => return db_error(error),
            };
            state
                .status_projection
                .ensure(
                    &session.key,
                    &created.session_id,
                    &session.connection_id,
                    &session.channel_id,
                    &session.root_thread_ts,
                )
                .await;
            (
                StatusCode::CREATED,
                Json(local_im_session_json(&session, "wait")),
            )
                .into_response()
        }
        Err(error) => {
            let _ = state.db.delete_session(&session.key);
            fail(error.status, &error.message)
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateLocalImSelection {
    profile_id: String,
    model: String,
    thinking: String,
}

async fn get_local_im_context(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> Response {
    local_im_context(&state, &session_id, None).await
}

async fn update_local_im_context(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    body: Result<Json<zork_agent_api::ContextConfig>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(config) = match body {
        Ok(config) => config,
        Err(_) => return fail(StatusCode::UNPROCESSABLE_ENTITY, "invalid_context"),
    };
    local_im_context(&state, &session_id, Some(&config)).await
}

async fn local_im_context(
    state: &AppState,
    session_id: &str,
    update: Option<&zork_agent_api::ContextConfig>,
) -> Response {
    if let Err(response) = local_im_session(state, session_id) {
        return *response;
    }
    match crate::agent::session_context(&state.config, session_id, update).await {
        Ok(config) => Json(config).into_response(),
        Err(error) => fail(error.status, &error.message),
    }
}

async fn update_local_im_selection(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Json(request): Json<UpdateLocalImSelection>,
) -> Response {
    let session = match local_im_session(&state, &session_id) {
        Ok(session) => session,
        Err(response) => return *response,
    };
    let selection = crate::agent::SessionSelection {
        profile_id: request.profile_id,
        model: request.model,
        thinking: request.thinking,
    };
    match crate::agent::update_selection(&state.config, &session_id, &selection).await {
        Ok(updated) => {
            if let Err(error) = state.db.set_agent_session(
                &session.key,
                &session_id,
                &session.workspace_path,
                &updated.profile_id,
                &updated.model,
                &updated.thinking,
            ) {
                return db_error(error);
            }
            match state.db.get_session(&session.key) {
                Ok(Some(session)) => Json(local_im_session_json(&session, "wait")).into_response(),
                Ok(None) => fail(StatusCode::INTERNAL_SERVER_ERROR, "binding_disappeared"),
                Err(error) => db_error(error),
            }
        }
        Err(error) => fail(error.status, &error.message),
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PostLocalImMessage {
    content: String,
}

async fn post_local_im_message(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Json(request): Json<PostLocalImMessage>,
) -> Response {
    if request.content.trim().is_empty() {
        return fail(StatusCode::BAD_REQUEST, "content is required");
    }
    let session = match local_im_session(&state, &session_id) {
        Ok(session) => session,
        Err(response) => return *response,
    };
    state
        .status_projection
        .ensure(
            &session.key,
            &session_id,
            &session.connection_id,
            &session.channel_id,
            &session.root_thread_ts,
        )
        .await;
    let message_id = ulid::Ulid::new().to_string();
    let message =
        match state
            .entries
            .accept_local_user_message(&session, &message_id, &request.content)
        {
            Ok(message) => message,
            Err(error) => return db_error(error),
        };
    if let Err(error) =
        crate::agent::append_mailbox(&state.config, &session_id, &request.content).await
    {
        return fail(StatusCode::BAD_GATEWAY, &error.to_string());
    }
    (
        StatusCode::ACCEPTED,
        Json(json!({ "ok": true, "message": visible_message_json(&message) })),
    )
        .into_response()
}

#[derive(Debug, Deserialize)]
struct LocalMessageQuery {
    before: Option<String>,
    limit: Option<i64>,
}

async fn list_local_im_messages(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Query(query): Query<LocalMessageQuery>,
) -> Response {
    let session = match local_im_session(&state, &session_id) {
        Ok(session) => session,
        Err(response) => return *response,
    };
    let limit = query.limit.unwrap_or(100);
    if !(1..=100).contains(&limit) {
        return fail(StatusCode::BAD_REQUEST, "limit must be between 1 and 100");
    }
    let before = match query.before {
        Some(cursor) => match cursor.parse::<i64>() {
            Ok(cursor) if cursor > 0 => Some(cursor),
            _ => return fail(StatusCode::BAD_REQUEST, "invalid message cursor"),
        },
        None => None,
    };
    match state.db.list_visible_messages(&session.key, before, limit) {
        Ok(messages) => {
            let older_cursor = messages.first().and_then(|message| {
                state
                    .db
                    .has_visible_messages_before(&session.key, message.sequence)
                    .ok()
                    .filter(|has_older| *has_older)
                    .map(|_| message.sequence.to_string())
            });
            Json(json!({
                "items": messages.iter().map(visible_message_json).collect::<Vec<_>>(),
                "older_cursor": older_cursor,
            }))
            .into_response()
        }
        Err(error) => db_error(error),
    }
}

async fn local_im_events(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> Response {
    let session = match local_im_session(&state, &session_id) {
        Ok(session) => session,
        Err(response) => return *response,
    };
    let receiver = state.entries.subscribe_local(&session.key);
    let events = futures_util::stream::unfold(receiver, |mut receiver| async move {
        loop {
            match receiver.recv().await {
                Ok(entry) => {
                    let event = Event::default()
                        .event(entry.name)
                        .data(entry.data.to_string());
                    return Some((Ok::<_, Infallible>(event), receiver));
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
            }
        }
    });
    Sse::new(events)
        .keep_alive(KeepAlive::default())
        .into_response()
}

async fn cancel_local_im_session(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> Response {
    if let Err(response) = local_im_session(&state, &session_id) {
        return *response;
    }
    match crate::agent::cancel_session(&state.config, &session_id).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => fail(StatusCode::NOT_FOUND, "agent_session_not_found"),
        Err(error) => fail(StatusCode::BAD_GATEWAY, &error.to_string()),
    }
}

fn local_im_session(
    state: &AppState,
    session_id: &str,
) -> Result<crate::db::SessionRow, Box<Response>> {
    match state.db.get_session_by_id(session_id) {
        Ok(Some(session)) if session.platform == LOCAL_GUI_PLATFORM => Ok(session),
        Ok(_) => Err(Box::new(fail(
            StatusCode::NOT_FOUND,
            "local_im_session_not_found",
        ))),
        Err(error) => Err(Box::new(db_error(error))),
    }
}

fn local_im_session_json(session: &crate::db::SessionRow, status: &str) -> Value {
    json!({
        "session_id": session.id,
        "profile_id": session.profile_id,
        "model": session.model,
        "thinking": session.thinking,
        "workspace": session.workspace_path,
        "status": if status == "working" { "working" } else { "wait" },
    })
}

pub fn gateway_router(state: AppState) -> Router {
    Router::new()
        .route("/readyz", get(readyz))
        .route("/healthz", get(readyz))
        .route("/sessions/{session_key}/im/bot", get(bot_identity))
        .route(
            "/sessions/{session_key}/im/threads/{conversation_id}/{root_message_id}",
            get(gateway_thread_history),
        )
        .route("/sessions/{session_key}/im/download", get(slack_download))
        .route(
            "/sessions/{session_key}/im/raw/{method}",
            post(slack_method),
        )
        .with_state(state)
}

async fn bot_identity(State(state): State<AppState>, Path(session_key): Path<String>) -> Response {
    let context = match bound_context(&state, &decode(&session_key)) {
        Ok(context) => context,
        Err(error) => return fail(StatusCode::NOT_FOUND, &error.to_string()),
    };
    let Some(connection) = state.connections.runtime(&context.connection_id).await else {
        return fail(StatusCode::NOT_FOUND, "session_connection_not_found");
    };
    match connection.slack.fetch_bot_self().await {
        Ok(bot) => {
            *connection.bot.lock().await = Some(crate::slack::BotSelf {
                user_id: bot.user_id.clone(),
                mention: bot.mention.clone(),
                raw: bot.raw.clone(),
            });
            Json(json!({ "ok": true, "self": bot.raw })).into_response()
        }
        Err(error) => fail(StatusCode::BAD_GATEWAY, &error.to_string()),
    }
}

#[derive(Debug, Deserialize)]
struct ThreadHistoryQuery {
    after: Option<String>,
    before: Option<String>,
    limit: Option<i64>,
}

async fn gateway_thread_history(
    State(state): State<AppState>,
    Path((session_key, conversation_id, root_message_id)): Path<(String, String, String)>,
    Query(query): Query<ThreadHistoryQuery>,
) -> Response {
    let context = match bound_context(&state, &decode(&session_key)) {
        Ok(context) => context,
        Err(error) => return fail(StatusCode::NOT_FOUND, &error.to_string()),
    };
    if let Err(error) = context.validate_destination(&conversation_id, &root_message_id) {
        return fail(StatusCode::BAD_REQUEST, &error.to_string());
    }
    let Some(connection) = state.connections.runtime(&context.connection_id).await else {
        return fail(StatusCode::NOT_FOUND, "session_connection_not_found");
    };
    let payload = match connection
        .slack
        .thread_history(
            &conversation_id,
            &root_message_id,
            query.before.as_deref(),
            query.limit,
        )
        .await
    {
        Ok(payload) => payload,
        Err(error) => return fail(StatusCode::BAD_GATEWAY, &error.to_string()),
    };
    let bot = connection.bot.lock().await;
    let bot_identity = bot.as_ref().map(|bot| zork_slack::BotIdentity {
        user_id: bot.user_id.clone(),
        bot_id: None,
        app_id: None,
        username: None,
        display_name: None,
        real_name: None,
        surface: "Slack".into(),
    });
    let messages = match payload.get("messages").and_then(Value::as_array) {
        Some(entries) => entries.clone(),
        None => return Json(payload).into_response(),
    };
    let parsed: Vec<Value> = match &bot_identity {
        Some(bot) => messages
            .iter()
            .filter_map(|message| {
                zork_slack::parse_history_message(&conversation_id, &root_message_id, message, bot)
            })
            .collect(),
        None => Vec::new(),
    };
    let after = query
        .after
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty());
    let filtered: Vec<Value> = parsed
        .into_iter()
        .filter(|parsed| {
            let Some(message_id) = parsed.get("messageId").and_then(Value::as_str) else {
                return false;
            };
            if after.is_some_and(|cursor| {
                slack_ts_cmp(message_id, cursor) != std::cmp::Ordering::Greater
            }) {
                return false;
            }
            true
        })
        .collect();
    Json(json!({ "ok": true, "messages": filtered })).into_response()
}

fn slack_ts_cmp(left: &str, right: &str) -> std::cmp::Ordering {
    match (left.parse::<f64>(), right.parse::<f64>()) {
        (Ok(left), Ok(right)) => left
            .partial_cmp(&right)
            .unwrap_or(std::cmp::Ordering::Equal),
        _ => left.cmp(right),
    }
}

async fn slack_method(
    State(state): State<AppState>,
    Path((session_key, method)): Path<(String, String)>,
    body: Bytes,
) -> Response {
    let context = match bound_context(&state, &decode(&session_key)) {
        Ok(context) => context,
        Err(error) => return fail(StatusCode::NOT_FOUND, &error.to_string()),
    };
    let Some(connection) = state.connections.runtime(&context.connection_id).await else {
        return fail(StatusCode::NOT_FOUND, "session_connection_not_found");
    };
    let method = method.trim_matches('/');
    if method.is_empty() || method.contains("..") || method.contains('/') {
        return fail(StatusCode::BAD_REQUEST, "invalid_method");
    }
    let fields = parse_form_fields(&body);
    let fields: Vec<(&str, &str)> = fields
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    match connection.slack.api().call(method, &fields).await {
        Ok(payload) => Json(payload).into_response(),
        Err(error) => fail(StatusCode::BAD_GATEWAY, &error.to_string()),
    }
}

fn parse_form_fields(body: &Bytes) -> Vec<(String, String)> {
    let raw = String::from_utf8_lossy(body);
    raw.split('&')
        .filter_map(|pair| {
            let (key, value) = pair.split_once('=')?;
            Some((urldecode(key), urldecode(value)))
        })
        .collect()
}

fn urldecode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                if let Ok(hex) = std::str::from_utf8(&bytes[index + 1..index + 3]) {
                    if let Ok(byte) = u8::from_str_radix(hex, 16) {
                        out.push(byte);
                        index += 3;
                        continue;
                    }
                }
                out.push(bytes[index]);
                index += 1;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[derive(Debug, Deserialize)]
struct DownloadQuery {
    url: String,
}

async fn slack_download(
    State(state): State<AppState>,
    Path(session_key): Path<String>,
    Query(query): Query<DownloadQuery>,
) -> Response {
    let context = match bound_context(&state, &decode(&session_key)) {
        Ok(context) => context,
        Err(error) => return fail(StatusCode::NOT_FOUND, &error.to_string()),
    };
    let Some(connection) = state.connections.runtime(&context.connection_id).await else {
        return fail(StatusCode::NOT_FOUND, "session_connection_not_found");
    };
    let api_base_url = connection
        .config
        .slack()
        .map(zork_config::SlackProviderConfig::api_base_url)
        .unwrap_or_default();
    if !is_allowed_slack_download(&query.url, &api_base_url) {
        return fail(StatusCode::BAD_REQUEST, "invalid_download_url");
    }
    match connection.slack.download(&query.url).await {
        Ok((bytes, content_type)) => (
            StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, content_type)],
            bytes,
        )
            .into_response(),
        Err(error) => fail(StatusCode::BAD_GATEWAY, &error.to_string()),
    }
}

fn is_allowed_slack_download(url: &str, slack_api_base_url: &str) -> bool {
    let Ok(target) = reqwest::Url::parse(url) else {
        return false;
    };
    if !matches!(target.scheme(), "https" | "http") {
        return false;
    }
    let host = target.host_str().unwrap_or_default();
    if host == "files.slack.com" || host == "slack-files.com" {
        return target.scheme() == "https";
    }
    let Ok(api) = reqwest::Url::parse(slack_api_base_url) else {
        return false;
    };
    api.scheme() == target.scheme()
        && api.host_str() == target.host_str()
        && api.port_or_known_default() == target.port_or_known_default()
}

pub async fn bind_listener(addr: std::net::SocketAddr) -> anyhow::Result<TcpListener> {
    let socket = if addr.is_ipv4() {
        tokio::net::TcpSocket::new_v4()?
    } else {
        tokio::net::TcpSocket::new_v6()?
    };
    socket.set_reuseaddr(true)?;
    #[cfg(unix)]
    socket.set_reuseport(true)?;
    socket.bind(addr)?;
    Ok(socket.listen(1024)?)
}

pub async fn serve_listener(listener: TcpListener, router: Router) -> anyhow::Result<()> {
    axum::serve(listener, router).await?;
    Ok(())
}

async fn readyz(State(state): State<AppState>) -> impl IntoResponse {
    Json(json!({
        "ok": true,
        "service": state.config.service_name,
        "pid": std::process::id(),
    }))
}

async fn fallback(State(state): State<AppState>) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({
            "ok": false,
            "service": state.config.service_name,
            "error": "route_not_found",
        })),
    )
        .into_response()
}

async fn snapshot(State(state): State<AppState>) -> Response {
    match state.db.snapshot() {
        Ok(value) => Json(value).into_response(),
        Err(error) => db_error(error),
    }
}

pub async fn list_sessions(State(state): State<AppState>) -> Response {
    match state.db.snapshot() {
        Ok(snapshot) => Json(json!({
            "ok": true,
            "realtime": snapshot.get("realtime"),
            "sessions": snapshot.pointer("/state/sessions").cloned().unwrap_or(json!([])),
        }))
        .into_response(),
        Err(error) => db_error(error),
    }
}

#[derive(Debug, Deserialize)]
pub struct LogsQuery {
    limit: Option<usize>,
}

pub async fn logs(State(state): State<AppState>, Query(query): Query<LogsQuery>) -> Response {
    Json(json!({
        "ok": true,
        "logs": read_recent_logs(&state.config.log_dir, query.limit.unwrap_or(40)),
    }))
    .into_response()
}

#[derive(Debug, Deserialize)]
pub struct PreflightQuery {
    operation: Option<String>,
}

pub async fn preflight(
    State(state): State<AppState>,
    Query(query): Query<PreflightQuery>,
) -> Response {
    match state
        .db
        .preflight(query.operation.as_deref().unwrap_or("unknown"))
    {
        Ok(value) => Json(value).into_response(),
        Err(error) => db_error(error),
    }
}

#[derive(Debug, Deserialize)]
pub struct EventsQuery {
    after: Option<i64>,
}

pub async fn events(
    State(state): State<AppState>,
    Query(query): Query<EventsQuery>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let mut cursor = query.after.unwrap_or(0);
    if cursor <= 0 {
        cursor = state.db.latest_admin_sequence().unwrap_or(0);
    }
    let db = state.db.clone();
    let events = futures_util::stream::unfold(cursor, move |cursor| {
        let db = db.clone();
        async move {
            tokio::time::sleep(Duration::from_millis(500)).await;
            let events = db.list_admin_events(cursor, 100).unwrap_or_default();
            let next = events
                .iter()
                .filter_map(|event| event.get("sequence").and_then(Value::as_i64))
                .max()
                .unwrap_or(cursor);
            let mut body = String::new();
            for event in events {
                let sequence = event
                    .get("sequence")
                    .and_then(Value::as_i64)
                    .unwrap_or(next);
                body.push_str(&format!(
                    "id: {sequence}\nevent: admin-event\ndata: {}\n\n",
                    json!({ "ok": true, "event": event })
                ));
            }
            Some((Ok(Event::default().data(body)), next))
        }
    });
    Sse::new(events).keep_alive(KeepAlive::default())
}

#[derive(Debug, Deserialize)]
pub struct TimelineQuery {
    limit: Option<usize>,
    before_sequence: Option<u64>,
}

pub async fn timeline(
    State(state): State<AppState>,
    Path(session_key): Path<String>,
    Query(query): Query<TimelineQuery>,
) -> Response {
    let session_key = decode(&session_key);
    let Some(binding) = state.db.get_binding(&session_key).ok().flatten() else {
        return not_found("session_not_found", &session_key);
    };
    let limit = query.limit.unwrap_or(100).clamp(1, 500);
    match timeline::load_page(&state.db, &binding, limit, query.before_sequence) {
        Ok(page) => match state.db.binding_summary(&binding) {
            Ok(summary) => Json(json!({
                "ok": true,
                "session": summary,
                "trace": page.get("summary"),
                "page": {
                    "limit": limit,
                    "hasMore": page.get("hasMore"),
                    "nextBeforeSequence": page.get("nextBeforeSequence"),
                },
                "events": page.get("events"),
            }))
            .into_response(),
            Err(error) => db_error(error),
        },
        Err(error) => db_error(error),
    }
}

pub async fn timeline_event(
    State(state): State<AppState>,
    Path((session_key, event_id)): Path<(String, String)>,
) -> Response {
    let session_key = decode(&session_key);
    let Some(binding) = state.db.get_binding(&session_key).ok().flatten() else {
        return not_found("session_not_found", &session_key);
    };
    match timeline::load_event(&state.db, &binding, &event_id) {
        Ok(Some(event)) => Json(json!({ "ok": true, "event": event })).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({
                "ok": false,
                "error": "trace_event_not_found",
                "sessionKey": session_key,
                "eventId": event_id
            })),
        )
            .into_response(),
        Err(error) => db_error(error),
    }
}

pub async fn reset_session(
    State(state): State<AppState>,
    Path(session_key): Path<String>,
) -> Response {
    let session_key = decode(&session_key);
    match delivery::reset_session(&state, &session_key).await {
        Ok(reset) => {
            Json(json!({ "ok": true, "sessionKey": session_key, "reset": reset })).into_response()
        }
        Err(error) => fail(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    }
}

pub async fn delete_session(
    State(state): State<AppState>,
    Path(session_key): Path<String>,
) -> Response {
    let session_key = decode(&session_key);
    match delivery::delete_session(&state, &session_key).await {
        Ok(deleted) => Json(json!({
            "ok": true,
            "sessionKey": session_key,
            "delete": deleted
        }))
        .into_response(),
        Err(error) => {
            let message = error.to_string();
            fail(
                if message.contains("Unknown session") {
                    StatusCode::NOT_FOUND
                } else {
                    StatusCode::INTERNAL_SERVER_ERROR
                },
                &message,
            )
        }
    }
}

async fn resolve_github_token(State(state): State<AppState>, Json(body): Json<Value>) -> Response {
    let Some(cwd) = read_string(&body, &["cwd"]) else {
        return missing(&["cwd"]);
    };
    let Some(binding) = state.db.find_binding_by_workspace(&cwd).ok().flatten() else {
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "ok": false,
                "mode": "blocked",
                "reason": "session_not_found",
                "message": format!("No Agent session is associated with {cwd}."),
            })),
        )
            .into_response();
    };
    let initiator_user_id = match &binding {
        crate::db::SessionBindingRow::Normal(session) => session.initiator_user_id.clone(),
        crate::db::SessionBindingRow::Proactive(_) => None,
    };
    if let Some(user_id) = &initiator_user_id {
        if let Some(mapping) = read_github_mapping(&state, user_id) {
            return Json(json!({
                "ok": true,
                "mode": "initiator",
                "slackUserId": user_id,
                "githubLogin": mapping.get("githubAuthor"),
                "token": mapping.get("token").cloned().or_else(|| state.config.default_github_token.clone().map(Value::String)),
            }))
            .into_response();
        }
    }
    if let (Some(login), Some(token)) = (
        state.config.default_github_login.as_ref(),
        state.config.default_github_token.as_ref(),
    ) {
        return Json(json!({
            "ok": true,
            "mode": "default",
            "defaultSource": "env",
            "githubLogin": login,
            "token": token,
            "reason": if initiator_user_id.is_some() { "initiator_unbound" } else { "missing_initiator" },
            "slackUserId": initiator_user_id,
        }))
        .into_response();
    }
    (
        StatusCode::CONFLICT,
        Json(json!({
            "ok": false,
            "mode": "blocked",
            "reason": "default_account_unavailable",
            "message": "No GitHub token is bound for this session.",
            "slackUserId": initiator_user_id,
        })),
    )
        .into_response()
}

async fn register_job(State(state): State<AppState>, Json(body): Json<Value>) -> Response {
    let session_key = read_string(&body, &["session_key", "sessionKey"]);
    let kind = read_string(&body, &["kind"]);
    let script = read_string(&body, &["script"]);
    let Some((session_key, kind, script)) = session_key
        .zip(kind)
        .zip(script)
        .map(|((a, b), c)| (a, b, c))
    else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "ok": false,
                "error": "missing_required_body",
                "required": ["sessionKey", "kind", "script"],
            })),
        )
            .into_response();
    };
    let cwd = read_string(&body, &["cwd"]);
    let shell = read_string(&body, &["shell"]);
    let restart = body
        .get("restart_on_boot")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    match state
        .jobs
        .register(
            &session_key,
            &kind,
            &script,
            cwd.as_deref(),
            shell.as_deref(),
            restart,
        )
        .await
    {
        Ok(job) => {
            Json(json!({ "ok": true, "job": JobSupervisor::job_json(&job) })).into_response()
        }
        Err(error) => fail(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    }
}

pub async fn cancel_job(
    State(state): State<AppState>,
    Path(job_id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let Some(session_key) = read_string(&body, &["session_key"]) else {
        return missing(&["session_key"]);
    };
    match state.jobs.cancel(&job_id, Some(&session_key)).await {
        Ok(job) => {
            Json(json!({ "ok": true, "job": JobSupervisor::job_json(&job) })).into_response()
        }
        Err(error) => {
            let message = error.to_string();
            fail(
                if message == "job_session_mismatch" {
                    StatusCode::BAD_REQUEST
                } else {
                    StatusCode::INTERNAL_SERVER_ERROR
                },
                &message,
            )
        }
    }
}

async fn notify(State(state): State<AppState>, Json(body): Json<Value>) -> Response {
    let session_key = read_string(&body, &["session_key", "sessionKey"]);
    let text = read_string(&body, &["text"]);
    let Some((session_key, text)) = session_key.zip(text) else {
        return missing(&["sessionKey", "text"]);
    };
    let job_id = read_string(&body, &["jobId", "job_id"]);
    match state
        .jobs
        .notify(job_id.as_deref(), &text, &session_key)
        .await
    {
        Ok(result) => Json(json!({ "ok": true, "result": result })).into_response(),
        Err(error) => {
            let message = error.to_string();
            let status = match message.as_str() {
                "session_not_found" | "job_not_found" => StatusCode::NOT_FOUND,
                "job_session_mismatch" => StatusCode::BAD_REQUEST,
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            };
            fail(status, &message)
        }
    }
}

#[derive(Debug, Deserialize)]
struct HistoryQuery {
    #[serde(rename = "sessionKey", alias = "session_key")]
    session_key: Option<String>,
    platform: Option<String>,
    conversation_id: Option<String>,
    #[serde(rename = "conversationId")]
    conversation_id_camel: Option<String>,
    root_message_id: Option<String>,
    #[serde(rename = "rootMessageId")]
    root_message_id_camel: Option<String>,
    before_message_id: Option<String>,
    #[serde(rename = "beforeMessageId")]
    before_message_id_camel: Option<String>,
    before_cursor: Option<String>,
    #[serde(rename = "beforeCursor")]
    before_cursor_camel: Option<String>,
    limit: Option<i64>,
    format: Option<String>,
}

async fn thread_history(
    State(state): State<AppState>,
    Query(query): Query<HistoryQuery>,
) -> Response {
    let Some(session_key) = query.session_key.as_deref() else {
        return missing(&["sessionKey"]);
    };
    let context = match bound_context(&state, session_key) {
        Ok(context) => context,
        Err(error) => return fail(StatusCode::NOT_FOUND, &error.to_string()),
    };
    let conversation_id = query
        .conversation_id
        .as_deref()
        .or(query.conversation_id_camel.as_deref());
    let root_message_id = query
        .root_message_id
        .as_deref()
        .or(query.root_message_id_camel.as_deref());
    let Some((conversation_id, root_message_id)) = conversation_id.zip(root_message_id) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "ok": false,
                "error": "missing_required_query",
                "required": ["sessionKey", "conversationId (alias: conversation_id)", "rootMessageId (alias: root_message_id)"],
            })),
        )
            .into_response();
    };
    if let Err(error) = context.validate_destination(conversation_id, root_message_id) {
        return fail(StatusCode::BAD_REQUEST, &error.to_string());
    }
    if query
        .platform
        .as_deref()
        .is_some_and(|platform| platform != context.platform)
    {
        return fail(StatusCode::BAD_REQUEST, "session_platform_mismatch");
    }
    if context.platform == LOCAL_GUI_PLATFORM {
        let before = query
            .before_message_id
            .as_deref()
            .or(query.before_message_id_camel.as_deref())
            .or(query.before_cursor.as_deref())
            .or(query.before_cursor_camel.as_deref());
        let before = match before {
            Some(value) => match value.parse::<i64>() {
                Ok(value) if value > 0 => Some(value),
                _ => return fail(StatusCode::BAD_REQUEST, "invalid message cursor"),
            },
            None => None,
        };
        let limit = query.limit.unwrap_or(50).clamp(1, 100);
        let messages = match state
            .db
            .list_visible_messages(&context.session_key, before, limit)
        {
            Ok(messages) => messages,
            Err(error) => return db_error(error),
        };
        let has_more = messages.first().is_some_and(|message| {
            state
                .db
                .has_visible_messages_before(&context.session_key, message.sequence)
                .unwrap_or(false)
        });
        let rows = messages
            .iter()
            .map(|message| {
                json!({
                    "messageId": message.sequence.to_string(),
                    "senderKind": message.role,
                    "text": message.text,
                    "kind": message.kind,
                    "createdAt": message.created_at,
                })
            })
            .collect::<Vec<_>>();
        let formatted = messages
            .iter()
            .map(|message| format!("{}: {}", message.role, message.text))
            .collect::<Vec<_>>()
            .join("\n\n");
        if query.format.as_deref() == Some("text") {
            return (
                StatusCode::OK,
                [("content-type", "text/plain; charset=utf-8")],
                if formatted.is_empty() {
                    "No earlier chat history matched the request.".to_owned()
                } else {
                    formatted
                },
            )
                .into_response();
        }
        return Json(json!({
            "ok": true,
            "platform": context.platform,
            "connectionId": context.connection_id,
            "conversationId": conversation_id,
            "rootMessageId": root_message_id,
            "returnedCount": rows.len(),
            "hasMore": has_more,
            "maxLimit": 100,
            "messages": rows,
            "formattedText": formatted,
        }))
        .into_response();
    }
    let Some(connection) = state.connections.runtime(&context.connection_id).await else {
        return fail(StatusCode::CONFLICT, "session_connection_unavailable");
    };
    let before = query
        .before_message_id
        .as_deref()
        .or(query.before_message_id_camel.as_deref())
        .or(query.before_cursor.as_deref())
        .or(query.before_cursor_camel.as_deref());
    match connection
        .slack
        .thread_history(
            conversation_id,
            root_message_id,
            before,
            query
                .limit
                .or(Some(state.config.slack_history_api_max_limit)),
        )
        .await
    {
        Ok(payload) => {
            if query.format.as_deref() == Some("text") {
                let text = payload
                    .get("formattedText")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
                    .unwrap_or_else(|| {
                        payload
                            .get("messages")
                            .map(ToString::to_string)
                            .unwrap_or_else(|| {
                                "No earlier chat history matched the request.".into()
                            })
                    });
                return (
                    StatusCode::OK,
                    [("content-type", "text/plain; charset=utf-8")],
                    text,
                )
                    .into_response();
            }
            Json(json!({
                "ok": true,
                "platform": context.platform,
                "connectionId": context.connection_id,
                "conversationId": conversation_id,
                "rootMessageId": root_message_id,
                "returnedCount": payload.get("messages").and_then(Value::as_array).map(Vec::len).unwrap_or(0),
                "hasMore": payload.get("hasMore").cloned().unwrap_or(json!(false)),
                "maxLimit": state.config.slack_history_api_max_limit,
                "messages": payload.get("messages").cloned().unwrap_or(json!([])),
                "formattedText": payload.get("formattedText"),
            }))
            .into_response()
        }
        Err(error) => fail(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    }
}

async fn post_message(State(state): State<AppState>, Json(body): Json<Value>) -> Response {
    let Some(session_key) = read_string(&body, &["session_key", "sessionKey"]) else {
        return missing(&["sessionKey"]);
    };
    let context = match bound_context(&state, &session_key) {
        Ok(context) => context,
        Err(error) => return fail(StatusCode::NOT_FOUND, &error.to_string()),
    };
    let conversation_id = read_string(&body, &["conversation_id", "conversationId"]);
    let root_message_id = read_string(&body, &["root_message_id", "rootMessageId"]);
    let text = read_string(&body, &["text"]);
    let Some(((conversation_id, root_message_id), text)) =
        conversation_id.zip(root_message_id).zip(text)
    else {
        return missing(&[
            "sessionKey",
            "conversationId (alias: conversation_id)",
            "rootMessageId (alias: root_message_id)",
            "text",
        ]);
    };
    if let Err(error) = context.validate_destination(&conversation_id, &root_message_id) {
        return fail(StatusCode::BAD_REQUEST, &error.to_string());
    }
    let kind = read_string(&body, &["kind"]);
    if let Some(kind) = kind.as_deref() {
        if !matches!(kind, "progress" | "final" | "block" | "wait") {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "ok": false, "error": "invalid_kind", "allowed": ["progress", "final", "block", "wait"] })),
            )
                .into_response();
        }
    }
    let reason = read_string(&body, &["reason", "stop_reason"]);
    if kind
        .as_deref()
        .is_some_and(|kind| matches!(kind, "block" | "wait"))
        && reason.is_none()
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(
                json!({ "ok": false, "error": "missing_reason", "requiredFor": ["block", "wait"] }),
            ),
        )
            .into_response();
    }
    match delivery::post_message(
        &state,
        &context.session_key,
        &conversation_id,
        &root_message_id,
        &text,
        kind.as_deref(),
    )
    .await
    {
        Ok(()) => Json(json!({
            "ok": true,
            "platform": context.platform,
            "connectionId": context.connection_id,
            "sessionKey": context.session_key,
            "conversationId": conversation_id,
            "rootMessageId": root_message_id,
        }))
        .into_response(),
        Err(error) => fail(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    }
}

async fn post_file(State(state): State<AppState>, Json(body): Json<Value>) -> Response {
    let Some(session_key) = read_string(&body, &["session_key", "sessionKey"]) else {
        return missing(&["sessionKey"]);
    };
    let context = match bound_context(&state, &session_key) {
        Ok(context) => context,
        Err(error) => return fail(StatusCode::NOT_FOUND, &error.to_string()),
    };
    let conversation_id = read_string(&body, &["conversation_id", "conversationId"]);
    let root_message_id = read_string(&body, &["root_message_id", "rootMessageId"]);
    let Some((conversation_id, root_message_id)) = conversation_id.zip(root_message_id) else {
        return missing(&["sessionKey", "conversationId", "rootMessageId"]);
    };
    if let Err(error) = context.validate_destination(&conversation_id, &root_message_id) {
        return fail(StatusCode::BAD_REQUEST, &error.to_string());
    }
    let Some(connection) = state.connections.runtime(&context.connection_id).await else {
        return fail(StatusCode::CONFLICT, "session_connection_unavailable");
    };
    let file_path = read_string(&body, &["file_path", "filePath"]);
    let content_b64 = read_string(&body, &["content_base64", "contentBase64"]);
    let (bytes, filename) = if let Some(path) = file_path {
        match fs::read(&path) {
            Ok(bytes) => (
                bytes,
                read_string(&body, &["filename"]).unwrap_or_else(|| {
                    StdPath::new(&path)
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or("file")
                        .to_string()
                }),
            ),
            Err(error) => return fail(StatusCode::BAD_REQUEST, &error.to_string()),
        }
    } else if let Some(content) = content_b64 {
        let Some(filename) = read_string(&body, &["filename"]) else {
            return missing(&["filename"]);
        };
        match base64::engine::general_purpose::STANDARD.decode(content) {
            Ok(bytes) => (bytes, filename),
            Err(error) => return fail(StatusCode::BAD_REQUEST, &error.to_string()),
        }
    } else {
        return fail(
            StatusCode::BAD_REQUEST,
            "Provide exactly one of file_path or content_base64",
        );
    };
    let title = read_string(&body, &["title"]);
    let comment = read_string(&body, &["initial_comment", "initialComment"]);
    match connection
        .slack
        .upload_file(
            &conversation_id,
            &root_message_id,
            &filename,
            &bytes,
            title.as_deref(),
            comment.as_deref(),
        )
        .await
    {
        Ok(payload) => Json(json!({ "ok": true, "file": payload })).into_response(),
        Err(error) => fail(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    }
}

struct BoundImContext {
    connection_id: String,
    platform: String,
    mode: zork_config::ImMode,
    session_key: String,
    conversation_id: Option<String>,
    root_message_id: Option<String>,
}

impl BoundImContext {
    fn validate_destination(
        &self,
        conversation_id: &str,
        root_message_id: &str,
    ) -> anyhow::Result<()> {
        if self.mode == zork_config::ImMode::Normal
            && (self.conversation_id.as_deref() != Some(conversation_id)
                || self.root_message_id.as_deref() != Some(root_message_id))
        {
            anyhow::bail!("session_destination_mismatch");
        }
        Ok(())
    }
}

fn bound_context(state: &AppState, session_key: &str) -> anyhow::Result<BoundImContext> {
    let binding = state
        .db
        .get_binding(session_key)?
        .context("session_not_found")?;
    Ok(BoundImContext {
        connection_id: binding.connection_id().to_owned(),
        platform: binding.platform().to_owned(),
        mode: binding.mode(),
        session_key: binding.key().to_owned(),
        conversation_id: binding.conversation_id().map(str::to_owned),
        root_message_id: binding.root_message_id().map(str::to_owned),
    })
}

#[derive(Debug, Deserialize)]
struct CliQuery {
    #[serde(rename = "threadId", alias = "thread_id")]
    thread_id: Option<String>,
    cwd: Option<String>,
}

async fn cli_context(State(state): State<AppState>, Query(query): Query<CliQuery>) -> Response {
    let thread_id = query
        .thread_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let binding = if let Some(thread_id) = thread_id {
        state.db.get_binding_by_id(thread_id).ok().flatten()
    } else if let Some(cwd) = query.cwd.as_deref() {
        state.db.find_binding_by_workspace(cwd).ok().flatten()
    } else {
        return fail(StatusCode::BAD_REQUEST, "missing_thread_id");
    };
    if let Some(binding) = binding {
        return Json(json!({
            "ok": true,
            "connectionId": binding.connection_id(),
            "platform": binding.platform(),
            "mode": binding.mode(),
            "conversationId": binding.conversation_id(),
            "rootMessageId": binding.root_message_id(),
            "channelId": binding.conversation_id(),
            "rootThreadTs": binding.root_message_id(),
            "sessionKey": binding.key(),
            "workspacePath": binding.workspace_path(),
        }))
        .into_response();
    }
    fail(StatusCode::NOT_FOUND, "unknown_thread")
}

#[derive(Debug, Deserialize)]
struct McpQuery {
    server: Option<String>,
}

async fn mcp_tools(State(_state): State<AppState>, Query(query): Query<McpQuery>) -> Response {
    let Some(server) = query.server.filter(|value| !value.trim().is_empty()) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "ok": false, "error": "missing_required_query", "required": ["server"] })),
        )
            .into_response();
    };
    Json(json!({ "ok": true, "server": server, "tools": [] })).into_response()
}

async fn mcp_call(State(_state): State<AppState>, Json(body): Json<Value>) -> Response {
    let Some(server) = read_string(&body, &["server"]) else {
        return missing(&["server"]);
    };
    let Some(name) = read_string(&body, &["name"]) else {
        return missing(&["name"]);
    };
    let arguments = body.get("arguments").cloned().unwrap_or(json!({}));
    let _ = (server, name, arguments);
    fail(
        StatusCode::NOT_IMPLEMENTED,
        "mcp integration is not configured",
    )
}

fn read_github_mapping(state: &AppState, slack_user_id: &str) -> Option<Value> {
    let path = state
        .config
        .github_mappings_dir()
        .join(format!("slack-{slack_user_id}.json"));
    fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
}

fn read_recent_logs(log_dir: &StdPath, limit: usize) -> Vec<Value> {
    let Ok(entries) = fs::read_dir(log_dir) else {
        return Vec::new();
    };
    let mut files: Vec<_> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
        .collect();
    files.sort();
    files.reverse();
    let mut records = Vec::new();
    for file in files {
        if records.len() >= limit {
            break;
        }
        let Ok(raw) = fs::read_to_string(file) else {
            continue;
        };
        for line in raw.lines().rev() {
            if records.len() >= limit {
                break;
            }
            if let Ok(value) = serde_json::from_str::<Value>(line) {
                records.push(value);
            }
        }
    }
    records.reverse();
    records
}

fn read_string(body: &Value, keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Some(value) = body.get(*key).and_then(Value::as_str) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

fn missing(required: &[&str]) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "ok": false, "error": "missing_required_body", "required": required })),
    )
        .into_response()
}

fn not_found(error: &str, session_key: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "ok": false, "error": error, "sessionKey": session_key })),
    )
        .into_response()
}

fn db_error(error: anyhow::Error) -> Response {
    fail(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string())
}

fn fail(status: StatusCode, error: &str) -> Response {
    (status, Json(json!({ "ok": false, "error": error }))).into_response()
}

fn decode(value: &str) -> String {
    percent_decode(value).unwrap_or_else(|| value.to_string())
}

fn percent_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).ok()?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                index += 3;
            }
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}
