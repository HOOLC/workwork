use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use zork_agent_api::{
    AgentProfile, ApiErrorBody, ApiErrorCode, CreateSessionRequest, DurableEvent, EventQuery,
    ItemList, MailboxRequest, MessageKind, MessagePage, MessageQuery, PublicMessage, PublicRole,
    SessionSelection, SessionStatus, SessionSummary, SessionView, TextDeltaEvent,
    DURABLE_EVENT_NAME, TEXT_DELTA_EVENT_NAME,
};

use crate::session::events::{SessionEvent, ToolOutcome, TurnOutcome};
use crate::session::ports::ProfileResolver;
use crate::session::query::QueryError;
use crate::session::service::{LiveSessionEvent, SessionService};
use crate::session::state::SessionState;
use crate::session::store::EventEnvelope;
use crate::session::supervisor::{PublicSlotStatus, SupervisorError};

const DEFAULT_MESSAGE_LIMIT: usize = 50;
const MAX_MESSAGE_LIMIT: usize = 200;
const SSE_QUERY_PAGE: usize = 200;
const SSE_SCAN_BUFFER: usize = 1;

pub struct AppState {
    pub service: Arc<SessionService>,
    pub profiles: Arc<crate::ProfileStore>,
    pub token: Option<String>,
}

pub fn router(state: AppState) -> Router {
    let token = state.token.clone();
    let router = Router::new()
        .route("/profiles", get(list_profiles))
        .route(
            "/profiles/{profile_id}",
            get(get_profile).put(put_profile).delete(delete_profile),
        )
        .route("/sessions", get(list_sessions).post(create_session))
        .route(
            "/sessions/{session_id}",
            get(get_session).delete(delete_session),
        )
        .route("/sessions/{session_id}/mailbox", post(append_mailbox))
        .route("/sessions/{session_id}/messages", get(list_messages))
        .route("/sessions/{session_id}/events", get(stream_events))
        .route("/sessions/{session_id}/cancel", post(cancel_session))
        .route(
            "/sessions/{session_id}/selection",
            axum::routing::put(set_selection),
        )
        .route(
            "/sessions/{session_id}/context",
            axum::routing::put(set_context),
        )
        .with_state(Arc::new(state));
    match token {
        Some(token) => router.route_layer(axum::middleware::from_fn_with_state(
            Arc::from(token),
            require_auth,
        )),
        None => router,
    }
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    code: ApiErrorCode,
    message: String,
}

impl ApiError {
    fn invalid(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            code: ApiErrorCode::InvalidRequest,
            message: message.into(),
        }
    }

    fn internal() -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: ApiErrorCode::InternalError,
            message: "internal error".into(),
        }
    }
}

impl From<SupervisorError> for ApiError {
    fn from(error: SupervisorError) -> Self {
        match error {
            SupervisorError::NotFound => Self {
                status: StatusCode::NOT_FOUND,
                code: ApiErrorCode::SessionNotFound,
                message: error.to_string(),
            },
            SupervisorError::SessionOverloaded => Self {
                status: StatusCode::TOO_MANY_REQUESTS,
                code: ApiErrorCode::SessionOverloaded,
                message: error.to_string(),
            },
            SupervisorError::GlobalOverloaded => Self {
                status: StatusCode::SERVICE_UNAVAILABLE,
                code: ApiErrorCode::GlobalOverloaded,
                message: error.to_string(),
            },
            SupervisorError::CircuitOpen(_) => Self {
                status: StatusCode::SERVICE_UNAVAILABLE,
                code: ApiErrorCode::RunnerCircuitOpen,
                message: error.to_string(),
            },
            SupervisorError::Deleting => Self {
                status: StatusCode::CONFLICT,
                code: ApiErrorCode::SessionDeleting,
                message: error.to_string(),
            },
            SupervisorError::Unavailable(_)
            | SupervisorError::Runner(_)
            | SupervisorError::Store(_)
            | SupervisorError::Query(_) => Self {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                code: ApiErrorCode::SessionUnavailable,
                message: error.to_string(),
            },
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ApiErrorBody::new(self.code, self.message)),
        )
            .into_response()
    }
}

async fn create_session(
    State(state): State<Arc<AppState>>,
    body: Result<Json<CreateSessionRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<(StatusCode, Json<SessionView>), ApiError> {
    let Json(request) = body.map_err(|_| ApiError::invalid("invalid JSON body"))?;
    let selection = selection(request.profile_id, request.model, request.thinking)?;
    validate_selection(&state.profiles, &selection)?;
    let workspace = request
        .workspace
        .unwrap_or_else(|| std::env::temp_dir().to_string_lossy().into_owned());
    let session_id = state
        .service
        .create_session(selection, request.system_prompt, workspace, request.context)
        .await
        .map_err(ApiError::from)?;
    let session = state
        .service
        .state(&session_id)
        .await
        .map_err(ApiError::from)?;
    Ok((StatusCode::CREATED, Json(session_view(&session))))
}

async fn list_sessions(State(state): State<Arc<AppState>>) -> Json<ItemList<SessionSummary>> {
    let items = state
        .service
        .sessions()
        .into_iter()
        .map(|slot| SessionSummary {
            session_id: slot.session_id,
            status: slot_status(slot.status, slot.finished),
        })
        .collect::<Vec<_>>();
    Json(ItemList { items })
}

async fn get_session(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
) -> Result<Json<SessionView>, ApiError> {
    let session = state
        .service
        .state(&session_id)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(session_view(&session)))
}

async fn delete_session(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    state
        .service
        .delete(&session_id)
        .await
        .map_err(ApiError::from)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn append_mailbox(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
    body: Result<Json<MailboxRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<StatusCode, ApiError> {
    let Json(request) = body.map_err(|_| ApiError::invalid("invalid JSON body"))?;
    if request.content.is_empty() {
        return Err(ApiError::invalid("content is required"));
    }
    state
        .service
        .submit_input(&session_id, request.content)
        .await
        .map_err(ApiError::from)?;
    Ok(StatusCode::ACCEPTED)
}

async fn cancel_session(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    state
        .service
        .cancel(&session_id)
        .await
        .map_err(ApiError::from)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn set_selection(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
    body: Result<Json<SessionSelection>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<SessionView>, ApiError> {
    let Json(body) = body.map_err(|_| ApiError::invalid("invalid JSON body"))?;
    let selection = selection(body.profile_id, body.model, body.thinking)?;
    validate_selection(&state.profiles, &selection)?;
    state
        .service
        .set_selection(&session_id, selection.clone())
        .await
        .map_err(ApiError::from)?;
    let session = state
        .service
        .state(&session_id)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(session_view(&session)))
}

async fn set_context(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
    body: Result<Json<zork_agent_api::ContextConfig>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<SessionView>, ApiError> {
    let Json(config) = body.map_err(|_| ApiError::invalid("invalid context configuration"))?;
    state
        .service
        .set_context(&session_id, config)
        .await
        .map_err(ApiError::from)?;
    let session = state
        .service
        .state(&session_id)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(session_view(&session)))
}

async fn list_messages(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
    query: Result<Query<MessageQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<MessagePage>, ApiError> {
    let Query(query) = query.map_err(|_| ApiError::invalid("invalid query"))?;
    if !state.service.contains(&session_id) {
        return Err(SupervisorError::NotFound.into());
    }
    let limit = query
        .limit
        .unwrap_or(DEFAULT_MESSAGE_LIMIT)
        .min(MAX_MESSAGE_LIMIT);
    if limit == 0 {
        return Ok(Json(MessagePage {
            items: Vec::new(),
            older_cursor: None,
        }));
    }

    let mut cursor = query.before;
    let mut messages = Vec::with_capacity(limit);
    while messages.len() < limit {
        let page = state
            .service
            .history_before(&session_id, cursor.as_deref(), SSE_QUERY_PAGE)
            .map_err(query_error)?;
        let Some(first) = page.first() else {
            break;
        };
        let next_cursor = first.event_id.clone();
        for envelope in page.iter().rev() {
            if let Some(message) = public_message(&envelope.event) {
                messages.push((envelope.event_id.clone(), message));
                if messages.len() == limit {
                    break;
                }
            }
        }
        if messages.len() == limit || page.len() < SSE_QUERY_PAGE {
            break;
        }
        cursor = Some(next_cursor);
    }
    let older_cursor = (messages.len() == limit)
        .then(|| messages.last().map(|(event_id, _)| event_id.clone()))
        .flatten();
    messages.reverse();
    Ok(Json(MessagePage {
        items: messages.into_iter().map(|(_, message)| message).collect(),
        older_cursor,
    }))
}

async fn stream_events(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
    query: Result<Query<EventQuery>, axum::extract::rejection::QueryRejection>,
    headers: HeaderMap,
) -> Result<Sse<impl futures_util::Stream<Item = Result<SseEvent, Infallible>>>, ApiError> {
    use async_stream::stream;

    let Query(query) = query.map_err(|_| ApiError::invalid("invalid query"))?;
    let cursor = headers
        .get("last-event-id")
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    let mut live = state
        .service
        .subscribe(&session_id)
        .map_err(ApiError::from)?;
    let service = state.service.clone();
    let mut initial_history = history_scan(service.clone(), session_id.clone(), cursor.clone());
    let first_history = match initial_history.recv().await {
        Some(HistoryScanItem::Event(envelope)) => HistoryScanItem::Event(envelope),
        Some(HistoryScanItem::Done) => HistoryScanItem::Done,
        Some(HistoryScanItem::Error(error)) => return Err(query_error(error)),
        None => return Err(ApiError::internal()),
    };

    let events = stream! {
        let mut last = cursor;
        let mut initial = Some((Some(first_history), initial_history));
        'catch_up: loop {
            let (mut next, mut history) = initial.take().unwrap_or_else(|| {
                (
                    None,
                    history_scan(service.clone(), session_id.clone(), last.clone()),
                )
            });
            loop {
                let item = match next.take() {
                    Some(item) => Some(item),
                    None => history.recv().await,
                };
                let envelope = match item {
                    Some(HistoryScanItem::Event(envelope)) => envelope,
                    Some(HistoryScanItem::Done) => break,
                    Some(HistoryScanItem::Error(_)) | None => return,
                };
                last = Some(envelope.event_id.clone());
                if let Some(event) = durable_sse_event(&envelope) {
                    yield Ok(event);
                }
            };

            loop {
                match live.recv().await {
                    Ok(LiveSessionEvent::Durable(envelope)) => {
                        if last.as_ref().is_some_and(|cursor| envelope.event_id <= *cursor) {
                            continue;
                        }
                        last = Some(envelope.event_id.clone());
                        if let Some(event) = durable_sse_event(envelope.as_ref()) {
                            yield Ok(event);
                        }
                    }
                    Ok(LiveSessionEvent::TextDelta { session_id, generation, step_id, text }) if query.transient => {
                        if let Ok(data) = serde_json::to_string(&TextDeltaEvent {
                            session_id,
                            generation,
                            step_id,
                            text,
                        }) {
                            yield Ok(SseEvent::default().event(TEXT_DELTA_EVENT_NAME).data(data));
                        }
                    }
                    Ok(LiveSessionEvent::TextDelta { .. }) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue 'catch_up,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                }
            }
        }
    };
    Ok(Sse::new(events).keep_alive(KeepAlive::default()))
}

enum HistoryScanItem {
    Event(Box<EventEnvelope>),
    Error(QueryError),
    Done,
}

fn history_scan(
    service: Arc<SessionService>,
    session_id: String,
    cursor: Option<String>,
) -> tokio::sync::mpsc::Receiver<HistoryScanItem> {
    let (sender, receiver) = tokio::sync::mpsc::channel(SSE_SCAN_BUFFER);
    tokio::task::spawn_blocking(move || {
        let result = service.scan_history_after(&session_id, cursor.as_deref(), &mut |event| {
            sender
                .blocking_send(HistoryScanItem::Event(Box::new(event)))
                .is_err()
        });
        let final_item = match result {
            Ok(()) => HistoryScanItem::Done,
            Err(error) => HistoryScanItem::Error(error),
        };
        let _ = sender.blocking_send(final_item);
    });
    receiver
}

fn durable_sse_event(envelope: &EventEnvelope) -> Option<SseEvent> {
    serde_json::to_string(&durable_event(envelope))
        .ok()
        .map(|data| {
            SseEvent::default()
                .event(DURABLE_EVENT_NAME)
                .id(envelope.event_id.clone())
                .data(data)
        })
}

fn public_message(event: &SessionEvent) -> Option<PublicMessage> {
    match event {
        SessionEvent::InputAppended { input } => Some(PublicMessage {
            kind: MessageKind::Message,
            role: PublicRole::Mailbox,
            content: input.content.clone(),
        }),
        SessionEvent::StepCompleted {
            assistant_text,
            purpose,
            ..
        } if *purpose == crate::session::events::Purpose::Conversation
            && !assistant_text.is_empty() =>
        {
            Some(PublicMessage {
                kind: MessageKind::Message,
                role: PublicRole::Assistant,
                content: assistant_text.clone(),
            })
        }
        SessionEvent::ToolResult { result } => Some(PublicMessage {
            kind: MessageKind::Message,
            role: PublicRole::Tool,
            content: render_tool_data(result.outcome, &result.data),
        }),
        _ => None,
    }
}

fn render_tool_data(outcome: ToolOutcome, data: &serde_json::Value) -> String {
    let content = data
        .as_str()
        .map(ToOwned::to_owned)
        .or_else(|| serde_json::to_string(data).ok())
        .unwrap_or_else(|| "null".into());
    if outcome == ToolOutcome::Succeeded {
        content
    } else {
        format!("{outcome:?}: {content}")
    }
}

fn session_view(state: &SessionState) -> SessionView {
    let selection = state.selection.as_ref();
    SessionView {
        session_id: state.session_id.clone(),
        profile_id: selection
            .map(|value| value.profile_id.clone())
            .unwrap_or_default(),
        model: selection
            .map(|value| value.model.clone())
            .unwrap_or_default(),
        thinking: selection
            .map(|value| value.thinking.clone())
            .unwrap_or_default(),
        workspace: state.workspace.clone(),
        generation: state.generation.number,
        context: state.context_config.clone(),
        status: state_status(state),
    }
}

fn state_status(state: &SessionState) -> SessionStatus {
    if state.active_step.is_some() {
        SessionStatus::Thinking
    } else if state.auto_wait.is_some() || state.wait_deadline.is_some() {
        SessionStatus::Waiting
    } else if state.active_turn.is_some()
        || (!state.pending_tools.is_empty()
            && state.last_turn_outcome != Some(TurnOutcome::Cancelled))
    {
        SessionStatus::Working
    } else {
        match state.last_turn_outcome {
            Some(TurnOutcome::Finished) => SessionStatus::Finished,
            Some(TurnOutcome::Failed) => SessionStatus::Failed,
            Some(TurnOutcome::Cancelled) => SessionStatus::Cancelled,
            None => SessionStatus::Wait,
        }
    }
}

fn slot_status(status: PublicSlotStatus, finished: bool) -> SessionStatus {
    match status {
        PublicSlotStatus::Active => SessionStatus::Working,
        PublicSlotStatus::Idle if finished => SessionStatus::Finished,
        PublicSlotStatus::Idle => SessionStatus::Wait,
        PublicSlotStatus::CircuitOpen => SessionStatus::Failed,
        PublicSlotStatus::Unavailable => SessionStatus::Unavailable,
        PublicSlotStatus::Deleting => SessionStatus::Deleting,
        PublicSlotStatus::Unchecked | PublicSlotStatus::Recovering => SessionStatus::Recovering,
    }
}

fn durable_event(envelope: &EventEnvelope) -> DurableEvent<&SessionEvent> {
    DurableEvent {
        event_id: envelope.event_id.clone(),
        schema_version: envelope.schema_version,
        batch_index: envelope.batch_index,
        batch_count: envelope.batch_count,
        event: &envelope.event,
    }
}

fn selection(
    profile_id: String,
    model: String,
    thinking: String,
) -> Result<crate::session::events::Selection, ApiError> {
    let profile_id = profile_id.trim().to_owned();
    let model = model.trim().to_owned();
    let thinking = thinking.trim().to_owned();
    if profile_id.is_empty() || model.is_empty() || thinking.is_empty() {
        return Err(ApiError::invalid(
            "profile_id, model, and thinking are required",
        ));
    }
    Ok(crate::session::events::Selection {
        profile_id,
        model,
        thinking,
    })
}

fn validate_selection(
    profiles: &crate::ProfileStore,
    selection: &crate::session::events::Selection,
) -> Result<(), ApiError> {
    match profiles.model_limits(selection) {
        Ok(_) => Ok(()),
        _ => Err(ApiError {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            code: ApiErrorCode::SelectionUnavailable,
            message: format!(
                "profile {} cannot resolve {}/{}",
                selection.profile_id, selection.model, selection.thinking
            ),
        }),
    }
}

fn query_error(error: QueryError) -> ApiError {
    match error {
        QueryError::InvalidSessionId(_) => SupervisorError::NotFound.into(),
        QueryError::InvalidCursor(_) | QueryError::UnknownCursor(_) => ApiError {
            status: StatusCode::BAD_REQUEST,
            code: ApiErrorCode::InvalidCursor,
            message: error.to_string(),
        },
        QueryError::Io(ref io) if io.kind() == std::io::ErrorKind::NotFound => {
            SupervisorError::NotFound.into()
        }
        _ => ApiError::internal(),
    }
}

async fn list_profiles(
    State(state): State<Arc<AppState>>,
) -> Result<Json<ItemList<AgentProfile>>, ApiError> {
    let profiles = state.profiles.clone();
    let items = tokio::task::spawn_blocking(move || profiles.list())
        .await
        .map_err(|_| ApiError::internal())?
        .map_err(|_| ApiError::internal())?;
    Ok(Json(ItemList { items }))
}

async fn get_profile(
    State(state): State<Arc<AppState>>,
    Path(profile_id): Path<String>,
) -> Result<Json<zork_profile::ProfileView>, ApiError> {
    let profiles = state.profiles.clone();
    let profile = tokio::task::spawn_blocking(move || profiles.get(&profile_id))
        .await
        .map_err(|_| ApiError::internal())?
        .map_err(|_| ApiError::internal())?
        .ok_or_else(|| ApiError {
            status: StatusCode::NOT_FOUND,
            code: ApiErrorCode::ProfileNotFound,
            message: "profile not found".into(),
        })?;
    Ok(Json(profile))
}

async fn put_profile(
    State(state): State<Arc<AppState>>,
    Path(profile_id): Path<String>,
    body: Result<Json<zork_agent_api::ProfileDocument>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<zork_profile::ProfileView>, ApiError> {
    let Json(body) = body.map_err(|_| ApiError::invalid("invalid JSON body"))?;
    let body = serde_json::to_value(body).map_err(|_| ApiError::internal())?;
    let profiles = state.profiles.clone();
    let write_id = profile_id.clone();
    tokio::task::spawn_blocking(move || profiles.put(&write_id, body))
        .await
        .map_err(|_| ApiError::internal())?
        .map_err(|error| ApiError::invalid(error.to_string()))?;
    state
        .profiles
        .refresh_status(&profile_id)
        .await
        .map_err(|_| ApiError::internal())?;
    let profiles = state.profiles.clone();
    let profile = tokio::task::spawn_blocking(move || profiles.get(&profile_id))
        .await
        .map_err(|_| ApiError::internal())?
        .map_err(|_| ApiError::internal())?
        .ok_or_else(ApiError::internal)?;
    Ok(Json(profile))
}

async fn delete_profile(
    State(state): State<Arc<AppState>>,
    Path(profile_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let profiles = state.profiles.clone();
    tokio::task::spawn_blocking(move || profiles.delete(&profile_id))
        .await
        .map_err(|_| ApiError::internal())?
        .map_err(|_| ApiError::internal())?;
    Ok(StatusCode::NO_CONTENT)
}

async fn require_auth(
    State(expected): State<Arc<str>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    use axum::http::header::AUTHORIZATION;
    let supplied = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    if supplied != Some(expected.as_ref()) {
        return ApiError {
            status: StatusCode::UNAUTHORIZED,
            code: ApiErrorCode::Unauthorized,
            message: "authentication required".into(),
        }
        .into_response();
    }
    next.run(request).await
}
