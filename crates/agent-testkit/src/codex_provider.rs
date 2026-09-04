use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::Message;

pub struct ControlledCodexProvider {
    base_url: String,
    requests: tokio::sync::mpsc::Receiver<PendingCodexRequest>,
    connections: Arc<Mutex<Vec<tokio::sync::mpsc::Sender<ServerCommand>>>>,
    connection_count: Arc<std::sync::atomic::AtomicUsize>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}

pub struct PendingCodexRequest {
    pub connection_id: usize,
    pub headers: BTreeMap<String, String>,
    pub body: Value,
    commands: tokio::sync::mpsc::Sender<ServerCommand>,
}

enum ServerCommand {
    SendJson(Value),
    Ping {
        payload: Vec<u8>,
        received: tokio::sync::oneshot::Sender<Vec<u8>>,
    },
    Close,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("the controlled Codex WebSocket connection is closed")]
pub struct CodexConnectionClosed;

impl ControlledCodexProvider {
    pub fn start(capacity: usize) -> std::io::Result<Self> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let address = listener.local_addr()?;
        let listener = tokio::net::TcpListener::from_std(listener)?;
        let (request_sender, requests) = tokio::sync::mpsc::channel(capacity.max(1));
        let connections = Arc::new(Mutex::new(Vec::new()));
        let connection_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (shutdown, mut stopped) = tokio::sync::oneshot::channel();
        let task_connections = connections.clone();
        let task_count = connection_count.clone();
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let Ok((stream, _)) = accepted else {
                            break;
                        };
                        let connection_id = task_count
                            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                            .saturating_add(1);
                        let requests = request_sender.clone();
                        let connections = task_connections.clone();
                        tokio::spawn(async move {
                            serve_connection(stream, connection_id, requests, connections).await;
                        });
                    }
                    _ = &mut stopped => break,
                }
            }
        });
        Ok(Self {
            base_url: format!("http://{address}/backend-api/codex"),
            requests,
            connections,
            connection_count,
            shutdown: Some(shutdown),
            task,
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn connection_count(&self) -> usize {
        self.connection_count
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    pub async fn request(&mut self) -> PendingCodexRequest {
        self.requests
            .recv()
            .await
            .expect("zork-agent stopped before issuing the expected Codex request")
    }

    pub async fn shutdown(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        let connections = self
            .connections
            .lock()
            .expect("controlled Codex connection lock poisoned")
            .clone();
        for connection in connections {
            let _ = connection.send(ServerCommand::Close).await;
        }
        let _ = self.task.await;
    }
}

impl PendingCodexRequest {
    pub async fn send_event(&self, event: Value) -> Result<(), CodexConnectionClosed> {
        self.commands
            .send(ServerCommand::SendJson(event))
            .await
            .map_err(|_| CodexConnectionClosed)
    }

    pub async fn respond(
        &self,
        response_id: &str,
        output: &[Value],
        input_tokens: u64,
        output_tokens: u64,
    ) -> Result<(), CodexConnectionClosed> {
        self.send_event(serde_json::json!({
            "type": "response.created",
            "response": {"id": response_id, "model": "controlled-codex-model"}
        }))
        .await?;
        for (output_index, item) in output.iter().enumerate() {
            self.send_event(serde_json::json!({
                "type": "response.output_item.done",
                "output_index": output_index,
                "item": item,
            }))
            .await?;
        }
        self.send_event(serde_json::json!({
            "type": "response.completed",
            "response": {
                "id": response_id,
                "object": "response",
                "model": "controlled-codex-model",
                "status": "completed",
                "output": output,
                "usage": {
                    "input_tokens": input_tokens,
                    "input_tokens_details": {"cached_tokens": input_tokens.saturating_sub(1)},
                    "output_tokens": output_tokens,
                    "output_tokens_details": {"reasoning_tokens": output_tokens.saturating_sub(1)}
                }
            }
        }))
        .await
    }

    pub async fn fail(
        &self,
        status: u16,
        code: &str,
        message: &str,
        request_id: &str,
    ) -> Result<(), CodexConnectionClosed> {
        self.send_event(serde_json::json!({
            "type": "error",
            "request_id": request_id,
            "error": {
                "status": status,
                "code": code,
                "message": message,
            }
        }))
        .await
    }

    pub async fn ping(&self, payload: &[u8]) -> Result<Vec<u8>, CodexConnectionClosed> {
        let (received, pong) = tokio::sync::oneshot::channel();
        self.commands
            .send(ServerCommand::Ping {
                payload: payload.to_vec(),
                received,
            })
            .await
            .map_err(|_| CodexConnectionClosed)?;
        pong.await.map_err(|_| CodexConnectionClosed)
    }
}

// `accept_hdr_async` fixes the callback error type to tungstenite's full HTTP
// response. The test server cannot replace or box that upstream type.
#[allow(clippy::result_large_err)]
async fn serve_connection(
    stream: tokio::net::TcpStream,
    connection_id: usize,
    requests: tokio::sync::mpsc::Sender<PendingCodexRequest>,
    connections: Arc<Mutex<Vec<tokio::sync::mpsc::Sender<ServerCommand>>>>,
) {
    let headers = Arc::new(Mutex::new(BTreeMap::new()));
    let captured_headers = headers.clone();
    let websocket = tokio_tungstenite::accept_hdr_async(
        stream,
        move |request: &Request, response: Response| {
            let mut headers = captured_headers
                .lock()
                .expect("controlled Codex handshake lock poisoned");
            headers.extend(request.headers().iter().filter_map(|(name, value)| {
                value
                    .to_str()
                    .ok()
                    .map(|value| (name.as_str().to_owned(), value.to_owned()))
            }));
            Ok(response)
        },
    )
    .await;
    let Ok(mut websocket) = websocket else {
        return;
    };
    let headers = headers
        .lock()
        .expect("controlled Codex handshake lock poisoned")
        .clone();
    let (commands, mut command_receiver) = tokio::sync::mpsc::channel(32);
    connections
        .lock()
        .expect("controlled Codex connection lock poisoned")
        .push(commands.clone());
    let mut pending_ping = None;
    loop {
        tokio::select! {
            command = command_receiver.recv() => match command {
                Some(ServerCommand::SendJson(event)) => {
                    if websocket.send(Message::Text(event.to_string().into())).await.is_err() {
                        break;
                    }
                }
                Some(ServerCommand::Ping { payload, received }) => {
                    if websocket.send(Message::Ping(payload.clone().into())).await.is_err() {
                        break;
                    }
                    pending_ping = Some((payload, received));
                }
                Some(ServerCommand::Close) | None => {
                    let _ = websocket.close(None).await;
                    break;
                }
            },
            message = websocket.next() => match message {
                Some(Ok(Message::Text(text))) => {
                    let Ok(body) = serde_json::from_str(&text) else {
                        break;
                    };
                    if requests.send(PendingCodexRequest {
                        connection_id,
                        headers: headers.clone(),
                        body,
                        commands: commands.clone(),
                    }).await.is_err() {
                        break;
                    }
                }
                Some(Ok(Message::Pong(payload))) => {
                    if let Some((expected, received)) = pending_ping.take() {
                        if payload.as_ref() == expected.as_slice() {
                            let _ = received.send(payload.to_vec());
                        }
                    }
                }
                Some(Ok(Message::Ping(payload))) => {
                    if websocket.send(Message::Pong(payload)).await.is_err() {
                        break;
                    }
                }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                Some(Ok(Message::Binary(_) | Message::Frame(_))) => {}
            }
        }
    }
}
