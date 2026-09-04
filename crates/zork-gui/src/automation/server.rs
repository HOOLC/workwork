use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use axum::extract::{Query, State};
use axum::http::header::{AUTHORIZATION, CACHE_CONTROL, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::{mpsc, oneshot};

use super::driver::{DriverCommand, DriverEnvelope, DriverError, DriverOutput, Screenshot};
use super::element::AutomationRegistry;
use super::protocol::{ElementsResponse, UserAction, API_VERSION, COORDINATE_SPACE};

const DRIVER_TIMEOUT: Duration = Duration::from_secs(15);
const DEFAULT_WAIT_TIMEOUT_MS: u64 = 10_000;
const MAX_WAIT_TIMEOUT_MS: u64 = 30_000;

pub(crate) struct ServerBinding {
    pub(crate) address: SocketAddr,
    pub(crate) receiver: mpsc::UnboundedReceiver<DriverEnvelope>,
    pub(crate) registry: AutomationRegistry,
    pub(crate) thread: JoinHandle<()>,
}

#[derive(Clone)]
struct ApiState {
    token: Arc<str>,
    driver: mpsc::UnboundedSender<DriverEnvelope>,
    registry: AutomationRegistry,
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

#[derive(Serialize)]
struct ErrorEnvelope<'a> {
    error: ErrorBody<'a>,
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    code: &'a str,
    message: &'a str,
}

impl ApiError {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }
}

impl From<DriverError> for ApiError {
    fn from(error: DriverError) -> Self {
        let status = match error.code {
            "element_not_found" => StatusCode::NOT_FOUND,
            "window_unavailable" => StatusCode::SERVICE_UNAVAILABLE,
            "screenshot_failed" | "screenshot_encode_failed" => StatusCode::INTERNAL_SERVER_ERROR,
            _ => StatusCode::BAD_REQUEST,
        };
        Self::new(status, error.code, error.message)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = Json(ErrorEnvelope {
            error: ErrorBody {
                code: self.code,
                message: &self.message,
            },
        });
        let mut response = (self.status, body).into_response();
        no_store(response.headers_mut());
        response
    }
}

#[derive(Default, Deserialize)]
struct ElementsQuery {
    #[serde(default)]
    include_hidden: bool,
    after_revision: Option<u64>,
    timeout_ms: Option<u64>,
}

pub(crate) fn bind(port: u16, token: String) -> std::io::Result<ServerBinding> {
    let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port)))?;
    listener.set_nonblocking(true)?;
    let address = listener.local_addr()?;
    let registry = AutomationRegistry::new();
    let (driver, receiver) = mpsc::unbounded_channel();
    let state = ApiState {
        token: Arc::from(token),
        driver,
        registry: registry.clone(),
    };
    let router = Router::new()
        .route("/health", get(health))
        .route("/v1", get(api_info))
        .route("/v1/elements", get(elements))
        .route("/v1/screenshot", get(screenshot))
        .route("/v1/actions", post(action))
        .with_state(state);
    let thread = thread::Builder::new()
        .name("zork-gui-dev-api".to_owned())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    eprintln!("zork-gui dev API runtime failed: {error}");
                    return;
                }
            };
            runtime.block_on(async move {
                let listener = match tokio::net::TcpListener::from_std(listener) {
                    Ok(listener) => listener,
                    Err(error) => {
                        eprintln!("zork-gui dev API listener failed: {error}");
                        return;
                    }
                };
                if let Err(error) = axum::serve(listener, router).await {
                    eprintln!("zork-gui dev API stopped: {error}");
                }
            });
        })?;

    Ok(ServerBinding {
        address,
        receiver,
        registry,
        thread,
    })
}

async fn health() -> Response {
    let mut response = Json(json!({
        "status": "ok",
        "dev_automation": true,
        "api_version": API_VERSION,
    }))
    .into_response();
    no_store(response.headers_mut());
    response
}

async fn api_info(State(state): State<ApiState>, headers: HeaderMap) -> Result<Response, ApiError> {
    authorize(&headers, &state)?;
    let mut response = Json(json!({
        "api_version": API_VERSION,
        "coordinate_space": COORDINATE_SPACE,
        "policy": "read rendered UI; mutate only through simulated mouse and keyboard input",
        "routes": {
            "elements": "GET /v1/elements",
            "screenshot": "GET /v1/screenshot",
            "actions": "POST /v1/actions"
        },
        "actions": ["click", "move", "type_text", "key", "scroll", "drag"]
    }))
    .into_response();
    no_store(response.headers_mut());
    Ok(response)
}

async fn elements(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Query(query): Query<ElementsQuery>,
) -> Result<Response, ApiError> {
    authorize(&headers, &state)?;
    let timed_out = if let Some(after_revision) = query.after_revision {
        wait_for_revision(
            &state.registry,
            after_revision,
            query.timeout_ms.unwrap_or(DEFAULT_WAIT_TIMEOUT_MS),
        )
        .await
    } else {
        false
    };
    let mut response = Json(ElementsResponse {
        snapshot: state.registry.snapshot(query.include_hidden),
        timed_out,
    })
    .into_response();
    no_store(response.headers_mut());
    Ok(response)
}

async fn screenshot(
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    authorize(&headers, &state)?;
    let output = request_driver(&state, DriverCommand::Screenshot).await?;
    let DriverOutput::Screenshot(screenshot) = output else {
        return Err(ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "unexpected_driver_response",
            "driver returned an action result for a screenshot request",
        ));
    };
    Ok(screenshot_response(screenshot))
}

async fn action(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Json(action): Json<UserAction>,
) -> Result<Response, ApiError> {
    authorize(&headers, &state)?;
    let output = request_driver(&state, DriverCommand::Action(action)).await?;
    let DriverOutput::Action(result) = output else {
        return Err(ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "unexpected_driver_response",
            "driver returned a screenshot for an action request",
        ));
    };
    let mut response = Json(result).into_response();
    no_store(response.headers_mut());
    Ok(response)
}

fn authorize(headers: &HeaderMap, state: &ApiState) -> Result<(), ApiError> {
    let authorized = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|token| token == state.token.as_ref());
    if authorized {
        Ok(())
    } else {
        Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "provide the dev token as an Authorization: Bearer header",
        ))
    }
}

async fn request_driver(
    state: &ApiState,
    command: DriverCommand,
) -> Result<DriverOutput, ApiError> {
    let (response, receiver) = oneshot::channel();
    state
        .driver
        .send(DriverEnvelope { command, response })
        .map_err(|_| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "driver_unavailable",
                "the GPUI event bridge is not running",
            )
        })?;
    tokio::time::timeout(DRIVER_TIMEOUT, receiver)
        .await
        .map_err(|_| {
            ApiError::new(
                StatusCode::GATEWAY_TIMEOUT,
                "driver_timeout",
                "the GPUI event bridge did not respond in time",
            )
        })?
        .map_err(|_| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "driver_unavailable",
                "the GPUI event bridge stopped before responding",
            )
        })?
        .map_err(ApiError::from)
}

async fn wait_for_revision(
    registry: &AutomationRegistry,
    after_revision: u64,
    timeout_ms: u64,
) -> bool {
    if registry.revision() > after_revision {
        return false;
    }
    let mut receiver = registry.subscribe();
    let timeout = Duration::from_millis(timeout_ms.clamp(1, MAX_WAIT_TIMEOUT_MS));
    let changed = async {
        loop {
            if *receiver.borrow_and_update() > after_revision {
                return;
            }
            if receiver.changed().await.is_err() {
                return;
            }
        }
    };
    tokio::time::timeout(timeout, changed).await.is_err()
}

fn screenshot_response(screenshot: Screenshot) -> Response {
    let mut response = screenshot.bytes.into_response();
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("image/png"));
    response.headers_mut().insert(
        "x-zork-ui-revision",
        HeaderValue::from_str(&screenshot.revision.to_string())
            .unwrap_or_else(|_| HeaderValue::from_static("0")),
    );
    response.headers_mut().insert(
        "x-zork-image-width",
        HeaderValue::from_str(&screenshot.width.to_string())
            .unwrap_or_else(|_| HeaderValue::from_static("0")),
    );
    response.headers_mut().insert(
        "x-zork-image-height",
        HeaderValue::from_str(&screenshot.height.to_string())
            .unwrap_or_else(|_| HeaderValue::from_static("0")),
    );
    no_store(response.headers_mut());
    response
}

fn no_store(headers: &mut HeaderMap) {
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
}
