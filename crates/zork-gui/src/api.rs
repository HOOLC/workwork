//! HTTP/SSE client for the gateway-owned local IM entry.
//!
//! Visible `message` events exist only after gateway ingress or an explicit
//! Agent `zork-call chat post-message`. Agent transcript events never cross
//! this API. `status` events carry non-message activity.

use bytes::Bytes;
use futures_channel::mpsc;
use futures_util::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use thiserror::Error;
pub use zork_config::{ContextConfig, ContextStrategy};

/// Roles deliberately delivered through the IM gateway.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
}

impl Role {
    pub fn label(self) -> &'static str {
        match self {
            Role::User => "you",
            Role::Assistant => "agent",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Wait,
    Working,
}

impl SessionStatus {
    pub fn label(self) -> &'static str {
        match self {
            SessionStatus::Wait => "wait",
            SessionStatus::Working => "working",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub session_id: String,
    pub profile_id: String,
    pub model: String,
    pub thinking: String,
    pub workspace: String,
    pub status: SessionStatus,
}

/// One item of a gateway `/messages` page or SSE `message` event.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TranscriptMessage {
    Message { role: Role, content: String },
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct MessagePage {
    pub items: Vec<TranscriptMessage>,
    pub older_cursor: Option<String>,
}

/// One item of `/v1/profiles`.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct ProfileInfo {
    pub profile_id: String,
    pub provider: String,
    #[serde(default)]
    pub models: Vec<ProfileModel>,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct ProfileModel {
    pub id: String,
    #[serde(default)]
    pub thinking: Vec<String>,
    #[serde(default)]
    pub default_thinking: String,
}

#[derive(Debug, Error)]
pub enum ApiError {
    #[error("request error: {0}")]
    Request(#[from] reqwest::Error),
    #[error("api error {status}: {message}")]
    Api { status: u16, message: String },
    #[error("gateway task failed: {0}")]
    Task(std::io::Error),
}

impl ApiError {
    pub fn status(&self) -> Option<u16> {
        match self {
            ApiError::Api { status, .. } => Some(*status),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub struct SseEvent {
    pub name: String,
    pub data: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct PublicToolCall {
    pub tool_call_id: String,
    pub tool_name: String,
}

/// Rich Agent activity projected by the gateway's `status` SSE events.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum AgentStatus {
    Clear,
    Thinking,
    ToolsStarted { calls: Vec<PublicToolCall> },
    ToolFinished { tool_call_id: String },
    Waiting { reason: String, deadline_ms: i64 },
    Failed { reason: String },
    Finished,
    Interrupted,
}

/// HTTP client for the gateway IM API.
///
/// All requests run on a dedicated Tokio runtime because reqwest's
/// connection pool requires a Tokio reactor, while the GUI's GPUI
/// background executor is not Tokio.
pub struct GatewayClient {
    http: reqwest::Client,
    sse_http: reqwest::Client,
    pub base_url: String,
    token: Option<String>,
    rt: tokio::runtime::Runtime,
}

fn join_task_error(err: tokio::task::JoinError) -> ApiError {
    ApiError::Task(std::io::Error::other(err.to_string()))
}

async fn send_request(
    http: &reqwest::Client,
    base_url: &str,
    token: &Option<String>,
    method: reqwest::Method,
    path: &str,
    body: Option<serde_json::Value>,
) -> Result<reqwest::Response, ApiError> {
    let mut request = http.request(method, format!("{base_url}{path}"));
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request.send().await?;
    if response.status().is_success() {
        return Ok(response);
    }
    let status = response.status().as_u16();
    let message = response
        .json::<serde_json::Value>()
        .await
        .ok()
        .and_then(|value| {
            value
                .pointer("/error/message")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| format!("status {status}"));
    Err(ApiError::Api { status, message })
}

impl GatewayClient {
    pub fn new(base_url: impl Into<String>, token: Option<String>) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .expect("reqwest client");
        // SSE is intentionally not given a total request timeout. Connection
        // establishment is bounded, but a healthy event stream is long-lived.
        let sse_http = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            .build()
            .expect("reqwest SSE client");
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("zork-gui-http")
            .enable_io()
            .enable_time()
            .build()
            .expect("failed to build tokio runtime");
        Self {
            http,
            sse_http,
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            token,
            rt,
        }
    }

    /// Runs `fut` on the dedicated runtime; the future must own all its state.
    async fn run_on<T, F>(&self, fut: F) -> Result<T, ApiError>
    where
        F: std::future::Future<Output = Result<T, ApiError>> + Send + 'static,
        T: Send + 'static,
    {
        self.rt.spawn(fut).await.map_err(join_task_error)?
    }

    pub async fn list_sessions(&self) -> Result<Vec<SessionSummary>, ApiError> {
        let http = self.http.clone();
        let base_url = self.base_url.clone();
        let token = self.token.clone();
        self.run_on(async move {
            let response = send_request(
                &http,
                &base_url,
                &token,
                reqwest::Method::GET,
                "/v1/im/sessions",
                None,
            )
            .await?;
            let body: serde_json::Value = response.json().await?;
            Ok(body["items"]
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| serde_json::from_value(item.clone()).ok())
                        .collect()
                })
                .unwrap_or_default())
        })
        .await
    }

    pub async fn create_session(
        &self,
        profile_id: &str,
        model: &str,
        thinking: &str,
        workspace: &str,
    ) -> Result<SessionSummary, ApiError> {
        let body = serde_json::json!({
            "profile_id": profile_id,
            "model": model,
            "thinking": thinking,
            "workspace": workspace,
        });
        let http = self.http.clone();
        let base_url = self.base_url.clone();
        let token = self.token.clone();
        self.run_on(async move {
            let response = send_request(
                &http,
                &base_url,
                &token,
                reqwest::Method::POST,
                "/v1/im/sessions",
                Some(body),
            )
            .await?;
            response.json().await.map_err(ApiError::from)
        })
        .await
    }

    pub async fn update_selection(
        &self,
        session_id: &str,
        profile_id: &str,
        model: &str,
        thinking: &str,
    ) -> Result<SessionSummary, ApiError> {
        let body = serde_json::json!({
            "profile_id": profile_id,
            "model": model,
            "thinking": thinking,
        });
        let path = format!("/v1/im/sessions/{session_id}/selection");
        let http = self.http.clone();
        let base_url = self.base_url.clone();
        let token = self.token.clone();
        self.run_on(async move {
            let response = send_request(
                &http,
                &base_url,
                &token,
                reqwest::Method::PUT,
                &path,
                Some(body),
            )
            .await?;
            response.json().await.map_err(ApiError::from)
        })
        .await
    }

    pub async fn session_context(
        &self,
        session_id: &str,
        update: Option<ContextConfig>,
    ) -> Result<ContextConfig, ApiError> {
        let path = format!("/v1/im/sessions/{session_id}/context");
        let http = self.http.clone();
        let base_url = self.base_url.clone();
        let token = self.token.clone();
        self.run_on(async move {
            let (method, body) = match update {
                Some(config) => (reqwest::Method::PUT, Some(serde_json::json!(config))),
                None => (reqwest::Method::GET, None),
            };
            let response = send_request(&http, &base_url, &token, method, &path, body).await?;
            response.json().await.map_err(ApiError::from)
        })
        .await
    }

    pub async fn post_message(&self, session_id: &str, content: &str) -> Result<(), ApiError> {
        let body = serde_json::json!({ "content": content });
        let path = format!("/v1/im/sessions/{session_id}/messages");
        let http = self.http.clone();
        let base_url = self.base_url.clone();
        let token = self.token.clone();
        self.run_on(async move {
            send_request(
                &http,
                &base_url,
                &token,
                reqwest::Method::POST,
                &path,
                Some(body),
            )
            .await?;
            Ok(())
        })
        .await
    }

    pub async fn cancel_session(&self, session_id: &str) -> Result<(), ApiError> {
        let path = format!("/v1/im/sessions/{session_id}/cancel");
        let http = self.http.clone();
        let base_url = self.base_url.clone();
        let token = self.token.clone();
        self.run_on(async move {
            send_request(&http, &base_url, &token, reqwest::Method::POST, &path, None).await?;
            Ok(())
        })
        .await
    }

    pub async fn list_messages(
        &self,
        session_id: &str,
        before: Option<&str>,
        limit: u32,
    ) -> Result<MessagePage, ApiError> {
        let mut query = format!("limit={limit}");
        if let Some(cursor) = before {
            let encoded = percent_encode(cursor);
            query.push_str(&format!("&before={encoded}"));
        }
        let path = format!("/v1/im/sessions/{session_id}/messages?{query}");
        let http = self.http.clone();
        let base_url = self.base_url.clone();
        let token = self.token.clone();
        self.run_on(async move {
            let response =
                send_request(&http, &base_url, &token, reqwest::Method::GET, &path, None).await?;
            response.json().await.map_err(ApiError::from)
        })
        .await
    }

    pub async fn list_profiles(&self) -> Result<Vec<ProfileInfo>, ApiError> {
        let http = self.http.clone();
        let base_url = self.base_url.clone();
        let token = self.token.clone();
        self.run_on(async move {
            let response = send_request(
                &http,
                &base_url,
                &token,
                reqwest::Method::GET,
                "/v1/im/profiles",
                None,
            )
            .await?;
            let body: serde_json::Value = response.json().await?;
            Ok(body["items"]
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| serde_json::from_value(item.clone()).ok())
                        .collect()
                })
                .unwrap_or_default())
        })
        .await
    }

    /// Open the SSE event stream for a session. The stream ends when the
    /// gateway closes the entry channel or the connection drops.
    pub async fn stream_events(&self, session_id: &str) -> Result<SseStream, ApiError> {
        let http = self.sse_http.clone();
        let base_url = self.base_url.clone();
        let token = self.token.clone();
        let path = format!("/v1/im/sessions/{session_id}/events");
        let (tx, rx) = mpsc::unbounded::<Result<Bytes, ApiError>>();
        let (ready_tx, ready_rx) = futures_channel::oneshot::channel();
        let task = self.rt.spawn(async move {
            let response =
                match send_request(&http, &base_url, &token, reqwest::Method::GET, &path, None)
                    .await
                {
                    Ok(response) => response,
                    Err(error) => {
                        let _ = ready_tx.send(Err(error));
                        return;
                    }
                };
            if ready_tx.send(Ok(())).is_err() {
                return;
            }
            let mut bytes = response.bytes_stream();
            while let Some(chunk) = bytes.next().await {
                match chunk {
                    Ok(bytes) => {
                        if tx.unbounded_send(Ok(bytes)).is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        let _ = tx.unbounded_send(Err(ApiError::Request(error)));
                        break;
                    }
                }
            }
        });
        let abort_handle = task.abort_handle();
        match ready_rx.await {
            Ok(Ok(())) => {
                // Dropping a Tokio JoinHandle detaches rather than cancels.
                // SseStream owns the abort handle and cancels on drop.
                drop(task);
            }
            Ok(Err(error)) => {
                let _ = task.await;
                return Err(error);
            }
            Err(_) => match task.await {
                Err(join_error) => return Err(join_task_error(join_error)),
                Ok(()) => {
                    return Err(ApiError::Task(std::io::Error::other(
                        "SSE task ended before response headers",
                    )))
                }
            },
        }
        Ok(SseStream::with_abort(rx, abort_handle))
    }
}

fn percent_encode(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Minimal SSE parser over a byte stream. Emits one `SseEvent` per blank
/// line; comment lines (`: ping` keepalives) are ignored. The upstream byte
/// stream is bridged from the dedicated Tokio runtime via an mpsc channel,
/// so `poll_next` can run on the GPUI executor.
pub struct SseStream {
    inner: futures_util::stream::BoxStream<'static, Result<Bytes, ApiError>>,
    buf: Vec<u8>,
    name: String,
    data: String,
    abort_handle: Option<tokio::task::AbortHandle>,
}

impl SseStream {
    pub fn new(
        inner: impl Stream<Item = Result<Bytes, ApiError>> + Unpin + Send + 'static,
    ) -> Self {
        Self {
            inner: inner.boxed(),
            buf: Vec::new(),
            name: String::new(),
            data: String::new(),
            abort_handle: None,
        }
    }

    fn with_abort(
        inner: impl Stream<Item = Result<Bytes, ApiError>> + Unpin + Send + 'static,
        abort_handle: tokio::task::AbortHandle,
    ) -> Self {
        let mut stream = Self::new(inner);
        stream.abort_handle = Some(abort_handle);
        stream
    }
}

impl Drop for SseStream {
    fn drop(&mut self) {
        if let Some(abort_handle) = self.abort_handle.take() {
            abort_handle.abort();
        }
    }
}

impl Stream for SseStream {
    type Item = Result<SseEvent, ApiError>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            if let Some(nl) = this.buf.iter().position(|byte| *byte == b'\n') {
                let mut line = this.buf.drain(..=nl).collect::<Vec<_>>();
                line.pop();
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                let line = String::from_utf8_lossy(&line);
                if line.is_empty() {
                    if !this.name.is_empty() || !this.data.is_empty() {
                        let name = if this.name.is_empty() {
                            "message".to_owned()
                        } else {
                            std::mem::take(&mut this.name)
                        };
                        let data = std::mem::take(&mut this.data);
                        return std::task::Poll::Ready(Some(Ok(SseEvent { name, data })));
                    }
                } else if let Some(value) = line.strip_prefix("event:") {
                    this.name = value.trim().to_owned();
                } else if let Some(value) = line.strip_prefix("data:") {
                    if !this.data.is_empty() {
                        this.data.push('\n');
                    }
                    this.data.push_str(value.trim());
                }
                // comment lines and unknown fields are ignored
                continue;
            }
            match this.inner.poll_next_unpin(cx) {
                std::task::Poll::Ready(Some(Ok(chunk))) => {
                    this.buf.extend_from_slice(&chunk);
                }
                std::task::Poll::Ready(Some(Err(error))) => {
                    return std::task::Poll::Ready(Some(Err(error)));
                }
                std::task::Poll::Ready(None) => return std::task::Poll::Ready(None),
                std::task::Poll::Pending => return std::task::Poll::Pending,
            }
        }
    }
}
