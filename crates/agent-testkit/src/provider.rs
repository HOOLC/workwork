use std::collections::BTreeMap;
use std::convert::Infallible;

use axum::body::{to_bytes, Body, Bytes};
use axum::extract::State;
use axum::http::{Request, Response, StatusCode};
use axum::routing::any;
use axum::Router;
use futures_util::stream;
use serde_json::Value;

pub struct ControlledHttpProvider {
    base_url: String,
    requests: tokio::sync::mpsc::Receiver<PendingHttpRequest>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}

pub struct PendingHttpRequest {
    pub method: String,
    pub path_and_query: String,
    pub headers: BTreeMap<String, String>,
    pub body: Bytes,
    response: tokio::sync::oneshot::Sender<ProviderResponse>,
}

pub struct ProviderSseStream {
    body: ProviderBodyStream,
}

pub struct ProviderBodyStream {
    chunks: tokio::sync::mpsc::Sender<ProviderChunk>,
}

struct ProviderState {
    requests: tokio::sync::mpsc::Sender<PendingHttpRequest>,
}

struct ProviderResponse {
    status: StatusCode,
    content_type: &'static str,
    body: ProviderBody,
}

enum ProviderBody {
    Full(Bytes),
    Stream(tokio::sync::mpsc::Receiver<ProviderChunk>),
}

struct ProviderChunk {
    bytes: Bytes,
    consumed: tokio::sync::oneshot::Sender<()>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("the controlled provider request is no longer waiting for a response")]
pub struct ProviderRequestClosed;

impl ControlledHttpProvider {
    pub fn start(capacity: usize) -> std::io::Result<Self> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let address = listener.local_addr()?;
        let listener = tokio::net::TcpListener::from_std(listener)?;
        let (sender, requests) = tokio::sync::mpsc::channel(capacity.max(1));
        let router = Router::new()
            .fallback(any(capture_request))
            .with_state(std::sync::Arc::new(ProviderState { requests: sender }));
        let (shutdown, stopped) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, router)
                .with_graceful_shutdown(async move {
                    let _ = stopped.await;
                })
                .await;
        });
        Ok(Self {
            base_url: format!("http://{address}"),
            requests,
            shutdown: Some(shutdown),
            task,
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub async fn request(&mut self) -> PendingHttpRequest {
        self.requests
            .recv()
            .await
            .expect("zork-agent stopped before issuing the expected provider request")
    }

    pub async fn shutdown(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        let _ = self.task.await;
    }
}

impl PendingHttpRequest {
    pub fn json(&self) -> serde_json::Result<Value> {
        serde_json::from_slice(&self.body)
    }

    pub fn respond_json(
        self,
        status: StatusCode,
        body: Value,
    ) -> Result<(), ProviderRequestClosed> {
        self.respond(ProviderResponse {
            status,
            content_type: "application/json",
            body: ProviderBody::Full(Bytes::from(body.to_string())),
        })
    }

    pub fn respond_sse<I>(self, events: I) -> Result<(), ProviderRequestClosed>
    where
        I: IntoIterator<Item = Value>,
    {
        let mut body = String::new();
        for event in events {
            body.push_str("data: ");
            body.push_str(&event.to_string());
            body.push_str("\n\n");
        }
        body.push_str("data: [DONE]\n\n");
        self.respond(ProviderResponse {
            status: StatusCode::OK,
            content_type: "text/event-stream",
            body: ProviderBody::Full(Bytes::from(body)),
        })
    }

    pub fn respond_openai_text(
        self,
        response_id: impl Into<String>,
        content: impl Into<String>,
    ) -> Result<(), ProviderRequestClosed> {
        let response_id = response_id.into();
        self.respond_sse([
            serde_json::json!({
                "id": response_id,
                "object": "chat.completion.chunk",
                "choices": [{
                    "index": 0,
                    "delta": {"role": "assistant", "content": content.into()}
                }]
            }),
            serde_json::json!({
                "id": response_id,
                "object": "chat.completion.chunk",
                "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 20, "completion_tokens": 2, "total_tokens": 22}
            }),
        ])
    }

    pub fn respond_openai_calls<I, C, T>(
        self,
        response_id: impl Into<String>,
        calls: I,
    ) -> Result<(), ProviderRequestClosed>
    where
        I: IntoIterator<Item = (C, T, Value)>,
        C: Into<String>,
        T: Into<String>,
    {
        let response_id = response_id.into();
        let calls = calls
            .into_iter()
            .enumerate()
            .map(|(index, (provider_call_id, tool, arguments))| {
                serde_json::json!({
                    "index": index,
                    "id": provider_call_id.into(),
                    "type": "function",
                    "function": {
                        "name": "call",
                        "arguments": serde_json::json!({
                            "tool": tool.into(),
                            "arguments": arguments
                        }).to_string()
                    }
                })
            })
            .collect::<Vec<_>>();
        self.respond_sse([
            serde_json::json!({
                "id": response_id,
                "object": "chat.completion.chunk",
                "choices": [{
                    "index": 0,
                    "delta": {"role": "assistant", "tool_calls": calls}
                }]
            }),
            serde_json::json!({
                "id": response_id,
                "object": "chat.completion.chunk",
                "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}],
                "usage": {"prompt_tokens": 20, "completion_tokens": 2, "total_tokens": 22}
            }),
        ])
    }

    pub fn begin_sse(self, capacity: usize) -> Result<ProviderSseStream, ProviderRequestClosed> {
        self.begin_stream(StatusCode::OK, "text/event-stream", capacity)
            .map(|body| ProviderSseStream { body })
    }

    pub fn begin_stream(
        self,
        status: StatusCode,
        content_type: &'static str,
        capacity: usize,
    ) -> Result<ProviderBodyStream, ProviderRequestClosed> {
        let (chunks, body) = tokio::sync::mpsc::channel(capacity.max(1));
        self.respond(ProviderResponse {
            status,
            content_type,
            body: ProviderBody::Stream(body),
        })?;
        Ok(ProviderBodyStream { chunks })
    }

    pub fn respond_raw(
        self,
        status: StatusCode,
        content_type: &'static str,
        body: impl Into<Bytes>,
    ) -> Result<(), ProviderRequestClosed> {
        self.respond(ProviderResponse {
            status,
            content_type,
            body: ProviderBody::Full(body.into()),
        })
    }

    fn respond(self, response: ProviderResponse) -> Result<(), ProviderRequestClosed> {
        self.response
            .send(response)
            .map_err(|_| ProviderRequestClosed)
    }
}

impl ProviderSseStream {
    pub async fn send_json(&self, event: Value) -> Result<(), ProviderRequestClosed> {
        self.send(format!("data: {event}\n\n")).await
    }

    pub async fn send(&self, chunk: impl Into<Bytes>) -> Result<(), ProviderRequestClosed> {
        self.body.send(chunk).await
    }

    pub async fn finish(self) -> Result<(), ProviderRequestClosed> {
        self.body.send("data: [DONE]\n\n").await?;
        self.body.finish().await
    }
}

impl ProviderBodyStream {
    pub async fn send(&self, chunk: impl Into<Bytes>) -> Result<(), ProviderRequestClosed> {
        let (consumed, received) = tokio::sync::oneshot::channel();
        self.chunks
            .send(ProviderChunk {
                bytes: chunk.into(),
                consumed,
            })
            .await
            .map_err(|_| ProviderRequestClosed)?;
        received.await.map_err(|_| ProviderRequestClosed)
    }

    pub async fn finish(self) -> Result<(), ProviderRequestClosed> {
        drop(self);
        Ok(())
    }
}

async fn capture_request(
    State(state): State<std::sync::Arc<ProviderState>>,
    request: Request<Body>,
) -> Response<Body> {
    let (parts, body) = request.into_parts();
    let body = match to_bytes(body, usize::MAX).await {
        Ok(body) => body,
        Err(_) => return status_response(StatusCode::BAD_REQUEST),
    };
    let headers = parts
        .headers
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_owned(), value.to_owned()))
        })
        .collect();
    let (response, received) = tokio::sync::oneshot::channel();
    if state
        .requests
        .send(PendingHttpRequest {
            method: parts.method.to_string(),
            path_and_query: parts
                .uri
                .path_and_query()
                .map(ToString::to_string)
                .unwrap_or_else(|| parts.uri.path().to_owned()),
            headers,
            body,
            response,
        })
        .await
        .is_err()
    {
        return status_response(StatusCode::SERVICE_UNAVAILABLE);
    }
    let Ok(response) = received.await else {
        return status_response(StatusCode::SERVICE_UNAVAILABLE);
    };
    let body = match response.body {
        ProviderBody::Full(body) => Body::from(body),
        ProviderBody::Stream(receiver) => {
            let stream = stream::unfold(receiver, |mut receiver| async move {
                receiver.recv().await.map(|chunk| {
                    let _ = chunk.consumed.send(());
                    (Ok::<_, Infallible>(chunk.bytes), receiver)
                })
            });
            Body::from_stream(stream)
        }
    };
    Response::builder()
        .status(response.status)
        .header("content-type", response.content_type)
        .body(body)
        .expect("controlled provider response is valid")
}

fn status_response(status: StatusCode) -> Response<Body> {
    Response::builder()
        .status(status)
        .body(Body::empty())
        .expect("status response is valid")
}
