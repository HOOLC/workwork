use std::{
    collections::HashMap,
    env,
    sync::{Arc, Mutex},
};

use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use futures_util::{SinkExt, StreamExt};
use percent_encoding::percent_decode_str;
use serde_json::{json, Map, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, Mutex as AsyncMutex};
use tokio_tungstenite::{
    client_async_tls, connect_async,
    tungstenite::{
        client::IntoClientRequest,
        http::{HeaderName, HeaderValue},
        Error as WebSocketError, Message,
    },
    MaybeTlsStream, WebSocketStream,
};
use url::{Host, Url};

use zork_agent::session::{
    model::{ModelError, ModelOutcome, ModelRequest, ModelTokenUsage, ProviderFailure},
    ports::ProfileExecution,
    wire::{
        ProviderContext, ProviderInputDiagnostics, ProviderInputMode, ProviderToolCall,
        TranscriptRole,
    },
};

const WEBSOCKET_BETA: &str = "responses_websockets=2026-02-06";
const MAX_PROXY_RESPONSE_HEADER_BYTES: usize = 16 * 1024;

fn retryable_status(status: u16) -> bool {
    !super::provider_failure_is_certainly_permanent(Some(status), None)
}

const WEBSOCKET_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

fn transport_failure(stage: &'static str, error: impl std::fmt::Display) -> ModelError {
    ModelError::ProviderFailed(ProviderFailure::new(stage, true, error.to_string()))
}

fn protocol_failure(stage: &'static str, message: impl Into<String>) -> ModelError {
    ModelError::ProviderFailed(ProviderFailure::new(stage, true, message))
}

fn status_failure(stage: &'static str, status: u16, message: impl Into<String>) -> ModelError {
    let mut failure = ProviderFailure::new(stage, retryable_status(status), message);
    failure.status_code = Some(status);
    ModelError::ProviderFailed(failure)
}

fn websocket_failure(stage: &'static str, error: WebSocketError) -> ModelError {
    let status_code = match &error {
        WebSocketError::Http(response) => Some(response.status().as_u16()),
        _ => None,
    };
    let mut failure = ProviderFailure::new(
        stage,
        status_code.is_none_or(retryable_status),
        error.to_string(),
    );
    failure.status_code = status_code;
    ModelError::ProviderFailed(failure)
}

fn provider_event_failure(event: &Value) -> ModelError {
    let error = event
        .get("error")
        .or_else(|| event.pointer("/response/error"));
    let incomplete_reason = event
        .pointer("/response/incomplete_details/reason")
        .and_then(Value::as_str);
    let status_code = error
        .and_then(|error| error.get("status").or_else(|| error.get("status_code")))
        .and_then(Value::as_u64)
        .and_then(|status| u16::try_from(status).ok());
    let provider_code = error
        .and_then(|error| error.get("code").or_else(|| error.get("type")))
        .and_then(Value::as_str)
        .or(incomplete_reason)
        .map(ToOwned::to_owned);
    let message = error
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .or_else(|| {
            incomplete_reason.map(|reason| format!("provider response incomplete: {reason}"))
        })
        .unwrap_or_else(|| "provider returned a failed response".to_owned());
    let request_id = event
        .get("request_id")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let retryable =
        !super::provider_failure_is_certainly_permanent(status_code, provider_code.as_deref());
    ModelError::ProviderFailed(ProviderFailure {
        stage: "codex.websocket.provider_event",
        retryable,
        status_code,
        provider_code,
        request_id,
        message,
        provider_input: None,
        usage: parse_usage(event.pointer("/response/usage")).map(Box::new),
    })
}

type RawCodexSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;

struct CodexSocket {
    tx_command: mpsc::Sender<CodexSocketCommand>,
    rx_message: mpsc::UnboundedReceiver<Result<Message, WebSocketError>>,
    pump_task: tokio::task::JoinHandle<()>,
}

enum CodexSocketCommand {
    Send {
        message: Message,
        tx_result: oneshot::Sender<Result<(), WebSocketError>>,
    },
}

impl CodexSocket {
    fn new(mut inner: RawCodexSocket) -> Self {
        let (tx_command, mut rx_command) = mpsc::channel::<CodexSocketCommand>(1);
        let (tx_message, rx_message) = mpsc::unbounded_channel::<Result<Message, WebSocketError>>();
        let pump_task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    command = rx_command.recv() => {
                        let Some(command) = command else {
                            break;
                        };
                        match command {
                            CodexSocketCommand::Send { message, tx_result } => {
                                let result = inner.send(message).await;
                                let should_break = result.is_err();
                                let _ = tx_result.send(result);
                                if should_break {
                                    break;
                                }
                            }
                        }
                    }
                    message = inner.next() => {
                        let Some(message) = message else {
                            break;
                        };
                        match message {
                            Ok(Message::Ping(payload)) => {
                                if let Err(error) = inner.send(Message::Pong(payload)).await {
                                    let _ = tx_message.send(Err(error));
                                    break;
                                }
                            }
                            Ok(Message::Pong(_)) => {}
                            Ok(message @ (Message::Text(_)
                            | Message::Binary(_)
                            | Message::Close(_)
                            | Message::Frame(_))) => {
                                let is_close = matches!(message, Message::Close(_));
                                if tx_message.send(Ok(message)).is_err() || is_close {
                                    break;
                                }
                            }
                            Err(error) => {
                                let _ = tx_message.send(Err(error));
                                break;
                            }
                        }
                    }
                }
            }
        });
        Self {
            tx_command,
            rx_message,
            pump_task,
        }
    }

    async fn send(&self, message: Message) -> Result<(), WebSocketError> {
        let (tx_result, rx_result) = oneshot::channel();
        self.tx_command
            .send(CodexSocketCommand::Send { message, tx_result })
            .await
            .map_err(|_| WebSocketError::ConnectionClosed)?;
        rx_result
            .await
            .unwrap_or(Err(WebSocketError::ConnectionClosed))
    }

    async fn next(&mut self) -> Option<Result<Message, WebSocketError>> {
        self.rx_message.recv().await
    }
}

impl Drop for CodexSocket {
    fn drop(&mut self) {
        self.pump_task.abort();
    }
}

pub(super) struct CodexResponsesProvider {
    sessions: Mutex<HashMap<String, Arc<AsyncMutex<SessionConnection>>>>,
}

impl CodexResponsesProvider {
    pub(super) fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
        }
    }

    pub(super) async fn complete(
        &self,
        request: &ModelRequest,
        execution: ProfileExecution,
    ) -> Result<ModelOutcome, ModelError> {
        if !execution.streaming() {
            if !request.independent {
                self.release_session(&request.session_id);
            }
            return Err(ModelError::InvalidSelection);
        }
        let logical = build_request(request, &execution)?;
        self.complete_websocket(logical, request, &execution).await
    }

    pub(super) fn release_session(&self, session_id: &str) {
        self.sessions
            .lock()
            .expect("Codex session map mutex poisoned")
            .remove(session_id);
    }

    async fn complete_websocket(
        &self,
        mut logical: LogicalRequest,
        request: &ModelRequest,
        execution: &ProfileExecution,
    ) -> Result<ModelOutcome, ModelError> {
        logical.body.insert("stream".to_owned(), Value::Bool(true));
        let identity = ConnectionIdentity::new(execution)?;
        let session = if request.independent {
            Arc::new(AsyncMutex::new(SessionConnection::default()))
        } else {
            let mut sessions = self
                .sessions
                .lock()
                .expect("Codex session map mutex poisoned");
            sessions
                .entry(request.session_id.clone())
                .or_insert_with(|| Arc::new(AsyncMutex::new(SessionConnection::default())))
                .clone()
        };
        let mut session = session.lock().await;
        if session.identity.as_ref() != Some(&identity) {
            session.reset();
        }
        if session.socket.is_none() {
            let socket = connect(&identity, request).await?;
            session.socket = Some(socket);
            session.identity = Some(identity);
        }

        let full_input = logical.input().to_vec();
        let properties = request_properties(&logical.body);
        let incremental = session
            .continuation
            .as_ref()
            .and_then(|continuation| continuation.delta(&properties, &full_input));
        let mut wire = logical.body.clone();
        wire.insert(
            "type".to_owned(),
            Value::String("response.create".to_owned()),
        );
        let mut provider_input = if let Some((previous_response_id, delta)) = incremental {
            wire.insert(
                "previous_response_id".to_owned(),
                Value::String(previous_response_id.to_owned()),
            );
            wire.insert("input".to_owned(), Value::Array(delta.to_vec()));
            ProviderInputDiagnostics {
                mode: ProviderInputMode::Delta,
                logical_input_items: u64::try_from(full_input.len())
                    .expect("usize fits in u64 on supported targets"),
                sent_input_items: u64::try_from(delta.len())
                    .expect("usize fits in u64 on supported targets"),
                previous_response_id: Some(previous_response_id.to_owned()),
                response_id: None,
            }
        } else {
            let input_items =
                u64::try_from(full_input.len()).expect("usize fits in u64 on supported targets");
            ProviderInputDiagnostics {
                mode: ProviderInputMode::Full,
                logical_input_items: input_items,
                sent_input_items: input_items,
                previous_response_id: None,
                response_id: None,
            }
        };

        let result = async {
            let socket = session.socket.as_mut().ok_or_else(|| {
                protocol_failure(
                    "codex.websocket.session_socket",
                    "session connection has no socket",
                )
            })?;
            socket
                .send(Message::Text(Value::Object(wire).to_string().into()))
                .await
                .map_err(|error| websocket_failure("codex.websocket.send", error))?;
            read_websocket_response(socket, request).await
        }
        .await;

        match result {
            Ok(completed) => {
                let output_items = completed.output_items;
                let response_id = completed.response_id;
                let mut outcome =
                    match outcome_from_items(output_items.clone(), completed.usage, execution) {
                        Ok(outcome) => outcome,
                        Err(error) => {
                            session.reset();
                            return Err(with_provider_input(error, provider_input));
                        }
                    };
                session.continuation = Some(Continuation {
                    properties,
                    request_input: full_input,
                    response_id: response_id.clone(),
                    response_items: output_items,
                });
                provider_input.response_id = Some(response_id);
                outcome.provider_input = Some(Box::new(provider_input));
                Ok(outcome)
            }
            Err(error) => {
                session.reset();
                Err(with_provider_input(error, provider_input))
            }
        }
    }
}

fn with_provider_input(
    mut error: ModelError,
    provider_input: ProviderInputDiagnostics,
) -> ModelError {
    if let ModelError::ProviderFailed(failure) = &mut error {
        failure.provider_input = Some(Box::new(provider_input));
    }
    error
}

#[derive(Default)]
struct SessionConnection {
    socket: Option<CodexSocket>,
    identity: Option<ConnectionIdentity>,
    continuation: Option<Continuation>,
}

impl SessionConnection {
    fn reset(&mut self) {
        self.socket = None;
        self.identity = None;
        self.continuation = None;
    }
}

#[derive(Clone, Eq, PartialEq)]
struct ConnectionIdentity {
    endpoint: String,
    proxy: Option<ProxyConfig>,
    profile_id: String,
    provider: String,
    model: String,
    headers: Vec<(String, String)>,
    secret: String,
}

impl ConnectionIdentity {
    fn new(execution: &ProfileExecution) -> Result<Self, ModelError> {
        let mut headers = execution
            .headers()
            .iter()
            .map(|(name, value)| (name.to_ascii_lowercase(), value.clone()))
            .collect::<Vec<_>>();
        headers.sort();
        let endpoint = websocket_endpoint(execution.base_url())?;
        let proxy = proxy_for_endpoint(&endpoint)?;
        Ok(Self {
            endpoint,
            proxy,
            profile_id: execution.profile_id().to_owned(),
            provider: execution.provider().to_owned(),
            model: execution.model().to_owned(),
            headers,
            secret: execution.secret().to_owned(),
        })
    }
}

#[derive(Clone, Eq, PartialEq)]
struct ProxyConfig {
    host: String,
    port: u16,
    authorization: Option<String>,
}

struct Continuation {
    properties: Value,
    request_input: Vec<Value>,
    response_id: String,
    response_items: Arc<Vec<Value>>,
}

impl Continuation {
    fn delta<'a>(
        &'a self,
        properties: &Value,
        current_input: &'a [Value],
    ) -> Option<(&'a str, &'a [Value])> {
        if &self.properties != properties || self.response_id.is_empty() {
            return None;
        }
        let baseline_len = self
            .request_input
            .len()
            .checked_add(self.response_items.len())?;
        if current_input.len() < baseline_len {
            return None;
        }
        let baseline_matches = self
            .request_input
            .iter()
            .chain(self.response_items.iter())
            .zip(current_input.iter())
            .all(|(previous, current)| previous == current);
        baseline_matches.then_some((&self.response_id, &current_input[baseline_len..]))
    }
}

struct LogicalRequest {
    body: Map<String, Value>,
}

impl LogicalRequest {
    fn input(&self) -> &[Value] {
        self.body
            .get("input")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .expect("logical Codex request always has input")
    }
}

fn build_request(
    request: &ModelRequest,
    execution: &ProfileExecution,
) -> Result<LogicalRequest, ModelError> {
    let mut input = Vec::new();
    if !request.tools.is_empty() {
        input.push(additional_tools(request));
    }
    for message in request.transcript.iter() {
        match message.role {
            TranscriptRole::System => input.push(text_message("developer", &message.content)),
            TranscriptRole::User => input.push(text_message("user", &message.content)),
            TranscriptRole::Assistant => {
                if let Some(context) = message
                    .provider_context
                    .as_ref()
                    .filter(|context| context_matches(context, execution))
                {
                    input.extend(context.output_items.iter().cloned());
                    continue;
                }
                if !message.content.is_empty() || message.tool_calls.is_empty() {
                    input.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "status": "completed",
                        "content": [{
                            "type": "output_text",
                            "text": message.content,
                            "annotations": [],
                        }],
                    }));
                }
                for call in &message.tool_calls {
                    input.push(json!({
                        "type": "function_call",
                        "status": "completed",
                        "call_id": call.tool_call_id,
                        "name": call.tool_name,
                        "namespace": "functions",
                        "arguments": serde_json::to_string(&call.arguments)
                            .map_err(|error| protocol_failure(
                                "codex.request.tool_arguments_json",
                                error.to_string(),
                            ))?,
                    }));
                }
            }
            TranscriptRole::Tool => {
                let tool_call_id = message.tool_call_id.as_deref().ok_or_else(|| {
                    protocol_failure(
                        "codex.request.tool_result_call_id",
                        "tool result has no tool call id",
                    )
                })?;
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": tool_call_id,
                    "output": message.content,
                }));
            }
        }
    }
    let mut body = Map::from_iter([
        (
            "model".to_owned(),
            Value::String(execution.model().to_owned()),
        ),
        ("store".to_owned(), Value::Bool(false)),
        ("input".to_owned(), Value::Array(input)),
        ("tool_choice".to_owned(), Value::String("auto".to_owned())),
        (
            "parallel_tool_calls".to_owned(),
            Value::Bool(execution.parallel_tool_calls()),
        ),
        (
            "reasoning".to_owned(),
            json!({
                "effort": execution.thinking(),
                "summary": "auto",
                "context": "all_turns",
            }),
        ),
        ("include".to_owned(), json!(["reasoning.encrypted_content"])),
        (
            "prompt_cache_key".to_owned(),
            Value::String(
                if request.independent {
                    &request.step_id
                } else {
                    &request.session_id
                }
                .clone(),
            ),
        ),
    ]);
    if let Some(service_tier) = execution.service_tier() {
        body.insert(
            "service_tier".to_owned(),
            Value::String(service_tier.to_owned()),
        );
    }
    Ok(LogicalRequest { body })
}

fn additional_tools(request: &ModelRequest) -> Value {
    let tools = request
        .tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "name": tool.name,
                "description": tool.description,
                "strict": false,
                "parameters": tool.input_schema,
            })
        })
        .collect::<Vec<_>>();
    json!({
        "type": "additional_tools",
        "role": "developer",
        "tools": [{
            "type": "namespace",
            "name": "functions",
            "description": "",
            "tools": tools,
        }],
    })
}

fn text_message(role: &str, text: &str) -> Value {
    json!({
        "type": "message",
        "role": role,
        "content": [{ "type": "input_text", "text": text }],
    })
}

fn context_matches(context: &ProviderContext, execution: &ProfileExecution) -> bool {
    context.profile_id == execution.profile_id()
        && context.provider == execution.provider()
        && context.model == execution.model()
        && context.api == execution.api()
}

fn request_properties(body: &Map<String, Value>) -> Value {
    let mut properties = body.clone();
    properties.remove("input");
    properties.remove("previous_response_id");
    Value::Object(properties)
}

async fn connect(
    identity: &ConnectionIdentity,
    request: &ModelRequest,
) -> Result<CodexSocket, ModelError> {
    let mut handshake = identity
        .endpoint
        .as_str()
        .into_client_request()
        .map_err(|_| ModelError::InvalidSelection)?;
    for (name, value) in &identity.headers {
        let name =
            HeaderName::from_bytes(name.as_bytes()).map_err(|_| ModelError::InvalidSelection)?;
        let value = HeaderValue::from_str(value).map_err(|_| ModelError::InvalidSelection)?;
        handshake.headers_mut().insert(name, value);
    }
    handshake.headers_mut().insert(
        "authorization",
        HeaderValue::from_str(&format!("Bearer {}", identity.secret))
            .map_err(|_| ModelError::InvalidSelection)?,
    );
    handshake
        .headers_mut()
        .insert("openai-beta", HeaderValue::from_static(WEBSOCKET_BETA));
    let routing_id = if request.independent {
        &request.step_id
    } else {
        &request.session_id
    };
    handshake.headers_mut().insert(
        "session-id",
        HeaderValue::from_str(routing_id).map_err(|_| ModelError::InvalidSelection)?,
    );
    handshake.headers_mut().insert(
        "x-client-request-id",
        HeaderValue::from_str(routing_id).map_err(|_| ModelError::InvalidSelection)?,
    );
    if let Some(proxy) = &identity.proxy {
        let mut stream = TcpStream::connect((proxy.host.as_str(), proxy.port))
            .await
            .map_err(|error| transport_failure("codex.proxy.connect", error))?;
        let authority = endpoint_authority(&identity.endpoint)?;
        establish_proxy_tunnel(&mut stream, &authority, proxy.authorization.as_deref()).await?;
        return client_async_tls(handshake, stream)
            .await
            .map(|(socket, _)| CodexSocket::new(socket))
            .map_err(|error| websocket_failure("codex.websocket.proxy_handshake", error));
    }
    connect_async(handshake)
        .await
        .map(|(socket, _)| CodexSocket::new(socket))
        .map_err(|error| websocket_failure("codex.websocket.connect", error))
}

fn proxy_for_endpoint(endpoint: &str) -> Result<Option<ProxyConfig>, ModelError> {
    let https_proxy = environment_value(&["HTTPS_PROXY", "https_proxy"])?;
    let http_proxy = environment_value(&["HTTP_PROXY", "http_proxy"])?;
    let no_proxy = environment_value(&["NO_PROXY", "no_proxy"])?;
    proxy_from_values(
        endpoint,
        https_proxy.as_deref(),
        http_proxy.as_deref(),
        no_proxy.as_deref(),
    )
}

fn environment_value(names: &[&str]) -> Result<Option<String>, ModelError> {
    for name in names {
        match env::var(name) {
            Ok(value) if !value.trim().is_empty() => return Ok(Some(value)),
            Ok(_) | Err(env::VarError::NotPresent) => {}
            Err(env::VarError::NotUnicode(_)) => return Err(ModelError::InvalidSelection),
        }
    }
    Ok(None)
}

fn proxy_from_values(
    endpoint: &str,
    https_proxy: Option<&str>,
    http_proxy: Option<&str>,
    no_proxy: Option<&str>,
) -> Result<Option<ProxyConfig>, ModelError> {
    let endpoint = Url::parse(endpoint).map_err(|_| ModelError::InvalidSelection)?;
    let host = endpoint.host_str().ok_or(ModelError::InvalidSelection)?;
    let port = endpoint
        .port_or_known_default()
        .ok_or(ModelError::InvalidSelection)?;
    if no_proxy.is_some_and(|list| no_proxy_matches(list, host, port)) {
        return Ok(None);
    }
    let value = match endpoint.scheme() {
        "wss" => https_proxy.or(http_proxy),
        "ws" => http_proxy,
        _ => return Err(ModelError::InvalidSelection),
    };
    let Some(value) = value else {
        return Ok(None);
    };
    let proxy = Url::parse(value).map_err(|_| ModelError::InvalidSelection)?;
    if proxy.scheme() != "http" {
        return Err(ModelError::InvalidSelection);
    }
    let proxy_host = proxy
        .host_str()
        .filter(|host| !host.is_empty())
        .ok_or(ModelError::InvalidSelection)?
        .to_owned();
    let proxy_port = proxy.port_or_known_default().unwrap_or(80);
    let authorization = if proxy.username().is_empty() && proxy.password().is_none() {
        None
    } else {
        let username = percent_decode_str(proxy.username())
            .decode_utf8()
            .map_err(|_| ModelError::InvalidSelection)?;
        let password = percent_decode_str(proxy.password().unwrap_or_default())
            .decode_utf8()
            .map_err(|_| ModelError::InvalidSelection)?;
        Some(format!(
            "Basic {}",
            BASE64_STANDARD.encode(format!("{username}:{password}"))
        ))
    };
    Ok(Some(ProxyConfig {
        host: proxy_host,
        port: proxy_port,
        authorization,
    }))
}

fn no_proxy_matches(list: &str, host: &str, port: u16) -> bool {
    list.split(',').any(|entry| {
        let entry = entry.trim();
        if entry == "*" {
            return true;
        }
        if entry.is_empty() {
            return false;
        }
        let (pattern, selected_port) = no_proxy_entry(entry);
        if selected_port.is_some_and(|selected| selected != port) {
            return false;
        }
        let pattern = pattern
            .strip_prefix("*.")
            .or_else(|| pattern.strip_prefix('.'))
            .unwrap_or(pattern);
        host.eq_ignore_ascii_case(pattern)
            || host
                .strip_suffix(pattern)
                .is_some_and(|prefix| prefix.ends_with('.'))
    })
}

fn no_proxy_entry(entry: &str) -> (&str, Option<u16>) {
    if entry.starts_with('[') {
        if let Some((host, port)) = entry.rsplit_once("]:") {
            return (
                host.strip_prefix('[').unwrap_or(host),
                port.parse::<u16>().ok(),
            );
        }
        return (entry.trim_matches(['[', ']']), None);
    }
    let Some((host, port)) = entry.rsplit_once(':') else {
        return (entry, None);
    };
    match port.parse::<u16>() {
        Ok(port) => (host, Some(port)),
        Err(_) => (entry, None),
    }
}

fn endpoint_authority(endpoint: &str) -> Result<String, ModelError> {
    let endpoint = Url::parse(endpoint).map_err(|_| ModelError::InvalidSelection)?;
    let host = endpoint.host().ok_or(ModelError::InvalidSelection)?;
    let port = endpoint
        .port_or_known_default()
        .ok_or(ModelError::InvalidSelection)?;
    Ok(match host {
        Host::Ipv6(address) => format!("[{address}]:{port}"),
        Host::Ipv4(address) => format!("{address}:{port}"),
        Host::Domain(domain) => format!("{domain}:{port}"),
    })
}

async fn establish_proxy_tunnel(
    stream: &mut TcpStream,
    authority: &str,
    authorization: Option<&str>,
) -> Result<(), ModelError> {
    let mut request = format!(
        "CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\nProxy-Connection: Keep-Alive\r\n"
    );
    if let Some(authorization) = authorization {
        request.push_str("Proxy-Authorization: ");
        request.push_str(authorization);
        request.push_str("\r\n");
    }
    request.push_str("\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|error| transport_failure("codex.proxy.write_connect", error))?;

    let mut response = Vec::with_capacity(512);
    let mut chunk = [0u8; 512];
    loop {
        let read = stream
            .read(&mut chunk)
            .await
            .map_err(|error| transport_failure("codex.proxy.read_connect", error))?;
        if read == 0 {
            return Err(transport_failure(
                "codex.proxy.read_connect",
                "proxy closed before returning CONNECT headers",
            ));
        }
        response.extend_from_slice(&chunk[..read]);
        if response.len() > MAX_PROXY_RESPONSE_HEADER_BYTES {
            return Err(protocol_failure(
                "codex.proxy.response_headers",
                "proxy CONNECT headers exceed the protocol limit",
            ));
        }
        if let Some(header_end) = response.windows(4).position(|window| window == b"\r\n\r\n") {
            response.truncate(header_end + 4);
            break;
        }
    }
    let response = std::str::from_utf8(&response).map_err(|error| {
        protocol_failure("codex.proxy.response_headers_utf8", error.to_string())
    })?;
    let status = response
        .split("\r\n")
        .next()
        .and_then(|line| {
            let mut fields = line.split_whitespace();
            let protocol = fields.next()?;
            let status = fields.next()?.parse::<u16>().ok()?;
            protocol.starts_with("HTTP/1.").then_some(status)
        })
        .ok_or_else(|| {
            protocol_failure(
                "codex.proxy.response_status",
                "proxy returned an invalid HTTP status line",
            )
        })?;
    if !(200..300).contains(&status) {
        return Err(status_failure(
            "codex.proxy.connect_status",
            status,
            format!("proxy CONNECT returned HTTP {status}"),
        ));
    }
    Ok(())
}

async fn read_websocket_response(
    socket: &mut CodexSocket,
    request: &ModelRequest,
) -> Result<CompletedResponse, ModelError> {
    read_websocket_response_with_idle(socket, request, WEBSOCKET_IDLE_TIMEOUT).await
}

/// 任何入帧（含 Ping/Pong）重置锚；生成期间后端事件流持续到达，长思考
/// 不被误杀；静默（连接死/服务卡）超窗即断。计时只活在请求读循环里——
/// 两个请求之间（工具执行期）的空闲缓存连接不受影响。
async fn read_websocket_response_with_idle(
    socket: &mut CodexSocket,
    request: &ModelRequest,
    idle_timeout: std::time::Duration,
) -> Result<CompletedResponse, ModelError> {
    let mut accumulator = StreamAccumulator::default();
    let mut last_frame = tokio::time::Instant::now();
    loop {
        let message = tokio::time::timeout_at(last_frame + idle_timeout, socket.next())
            .await
            .map_err(|_| {
                transport_failure(
                    "codex.websocket.idle",
                    format!(
                        "no inbound frame for {}s while awaiting response",
                        idle_timeout.as_secs()
                    ),
                )
            })?;
        let Some(message) = message else {
            return Err(transport_failure(
                "codex.websocket.eof",
                "provider websocket ended before response.completed",
            ));
        };
        last_frame = tokio::time::Instant::now();
        match message.map_err(|error| websocket_failure("codex.websocket.read", error))? {
            Message::Text(text) => {
                let event = serde_json::from_str::<Value>(&text).map_err(|error| {
                    protocol_failure("codex.websocket.event_json", error.to_string())
                })?;
                if let Some(completed) = accumulator.observe(event, request)? {
                    return Ok(completed);
                }
            }
            Message::Ping(_) | Message::Pong(_) => {}
            Message::Close(frame) => {
                return Err(transport_failure(
                    "codex.websocket.closed",
                    frame.map_or_else(
                        || "provider closed the websocket".to_owned(),
                        |frame| format!("provider closed the websocket: {}", frame.code),
                    ),
                ))
            }
            Message::Binary(_) | Message::Frame(_) => {
                return Err(protocol_failure(
                    "codex.websocket.message_type",
                    "provider returned a non-text websocket message",
                ))
            }
        }
    }
}

#[derive(Default)]
struct StreamAccumulator {
    response_id: Option<String>,
    output_items: Vec<Option<Value>>,
}

impl StreamAccumulator {
    fn observe(
        &mut self,
        mut event: Value,
        request: &ModelRequest,
    ) -> Result<Option<CompletedResponse>, ModelError> {
        match event.get("type").and_then(Value::as_str) {
            Some("response.created") => {
                self.response_id = event
                    .pointer("/response/id")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
                Ok(None)
            }
            Some("response.output_text.delta") => {
                if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                    request.stream_observer.text_delta(
                        &request.session_id,
                        request.generation,
                        &request.step_id,
                        delta,
                    );
                }
                Ok(None)
            }
            Some("response.output_item.done") => {
                let index = event
                    .get("output_index")
                    .and_then(Value::as_u64)
                    .and_then(|index| usize::try_from(index).ok())
                    .ok_or_else(|| {
                        protocol_failure(
                            "codex.event.output_item_index",
                            "response.output_item.done has no valid output_index",
                        )
                    })?;
                let item = event
                    .get("item")
                    .filter(|item| item.is_object())
                    .cloned()
                    .ok_or_else(|| {
                        protocol_failure(
                            "codex.event.output_item",
                            "response.output_item.done has no object item",
                        )
                    })?;
                if self.output_items.len() <= index {
                    self.output_items.resize(index + 1, None);
                }
                if self.output_items[index].replace(item).is_some() {
                    return Err(protocol_failure(
                        "codex.event.output_item_duplicate",
                        "provider completed the same output index more than once",
                    ));
                }
                Ok(None)
            }
            Some("response.completed") => {
                let response = event
                    .get("response")
                    .filter(|response| response.is_object())
                    .ok_or_else(|| {
                        protocol_failure(
                            "codex.event.completed_response",
                            "response.completed has no response object",
                        )
                    })?;
                if response.get("status").and_then(Value::as_str) != Some("completed") {
                    return Err(protocol_failure(
                        "codex.event.completed_status",
                        "response.completed carries a non-completed status",
                    ));
                }
                let response_id = response
                    .get("id")
                    .and_then(Value::as_str)
                    .or(self.response_id.as_deref())
                    .filter(|id| !id.is_empty())
                    .ok_or_else(|| {
                        protocol_failure(
                            "codex.event.response_id",
                            "completed response has no response id",
                        )
                    })?
                    .to_owned();
                let usage = parse_usage(response.get("usage"));
                let output_items = if self.output_items.is_empty() {
                    event
                        .pointer_mut("/response/output")
                        .and_then(Value::as_array_mut)
                        .map(std::mem::take)
                        .unwrap_or_default()
                } else {
                    self.output_items
                        .drain(..)
                        .map(|item| {
                            item.ok_or_else(|| {
                                protocol_failure(
                                    "codex.event.output_item_gap",
                                    "completed response has a missing output item",
                                )
                            })
                        })
                        .collect::<Result<Vec<_>, _>>()?
                };
                Ok(Some(CompletedResponse {
                    response_id,
                    output_items: Arc::new(output_items),
                    usage,
                }))
            }
            Some("response.failed" | "response.incomplete" | "error") => {
                Err(provider_event_failure(&event))
            }
            _ => Ok(None),
        }
    }
}

struct CompletedResponse {
    response_id: String,
    output_items: Arc<Vec<Value>>,
    usage: Option<ModelTokenUsage>,
}

fn outcome_from_items(
    output_items: Arc<Vec<Value>>,
    usage: Option<ModelTokenUsage>,
    execution: &ProfileExecution,
) -> Result<ModelOutcome, ModelError> {
    let mut text = String::new();
    let mut tool_calls = Vec::new();
    for item in output_items.iter() {
        match item.get("type").and_then(Value::as_str) {
            Some("reasoning")
                if item
                    .get("encrypted_content")
                    .and_then(Value::as_str)
                    .is_none_or(str::is_empty) =>
            {
                return Err(protocol_failure(
                    "codex.output.reasoning_encrypted_content",
                    "reasoning output has no encrypted content",
                ));
            }
            Some("reasoning") => {}
            Some("message") => {
                for content in item
                    .get("content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if content.get("type").and_then(Value::as_str) == Some("output_text") {
                        let part =
                            content.get("text").and_then(Value::as_str).ok_or_else(|| {
                                protocol_failure(
                                    "codex.output.text",
                                    "output_text item has no text",
                                )
                            })?;
                        text.push_str(part);
                    }
                }
            }
            Some("function_call") => {
                let tool_call_id = item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let tool_name = item
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let arguments = match item.get("arguments") {
                    Some(Value::String(raw)) => serde_json::from_str::<Value>(raw)
                        .unwrap_or_else(|_| Value::String(raw.to_owned())),
                    Some(raw) => Value::Array(vec![raw.clone()]),
                    None => Value::Null,
                };
                tool_calls.push(ProviderToolCall {
                    tool_call_id,
                    tool_name,
                    arguments,
                });
            }
            _ => {}
        }
    }
    let provider_context = Some(ProviderContext {
        profile_id: execution.profile_id().to_owned(),
        provider: execution.provider().to_owned(),
        model: execution.model().to_owned(),
        api: execution.api().to_owned(),
        output_items,
    });
    Ok(ModelOutcome {
        text,
        tool_calls,
        provider_context,
        usage,
        provider_input: None,
    })
}

fn parse_usage(usage: Option<&Value>) -> Option<ModelTokenUsage> {
    let usage = usage?;
    let input_tokens = usage.get("input_tokens")?.as_u64()?;
    let output_tokens = usage.get("output_tokens")?.as_u64()?;
    let cached_input_tokens = usage
        .pointer("/input_tokens_details/cached_tokens")
        .and_then(Value::as_u64);
    let output_reasoning_tokens = usage
        .pointer("/output_tokens_details/reasoning_tokens")
        .and_then(Value::as_u64);
    let output_text_tokens =
        output_reasoning_tokens.and_then(|reasoning| output_tokens.checked_sub(reasoning));
    Some(ModelTokenUsage {
        input_tokens,
        cached_input_tokens,
        output_tokens,
        output_reasoning_tokens,
        output_text_tokens,
    })
}

fn responses_endpoint(base_url: &str) -> Result<String, ModelError> {
    let mut url = url::Url::parse(base_url).map_err(|_| ModelError::InvalidSelection)?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(ModelError::InvalidSelection);
    }
    let path = url.path().trim_end_matches('/');
    if !path.ends_with("/responses") {
        url.set_path(&format!("{path}/responses"));
    }
    Ok(url.to_string().trim_end_matches('/').to_owned())
}

fn websocket_endpoint(base_url: &str) -> Result<String, ModelError> {
    let endpoint = responses_endpoint(base_url)?;
    let mut url = url::Url::parse(&endpoint).map_err(|_| ModelError::InvalidSelection)?;
    match url.scheme() {
        "http" => url
            .set_scheme("ws")
            .map_err(|_| ModelError::InvalidSelection)?,
        "https" => url
            .set_scheme("wss")
            .map_err(|_| ModelError::InvalidSelection)?,
        _ => return Err(ModelError::InvalidSelection),
    }
    Ok(url.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    // Contract: docs/zork-agent-architecture.md [PROVIDER-01]
    fn request_uses_codex_wire_contract_and_profile_parallelism() {
        let execution = ProfileExecution::new(
            "profile".into(),
            "openai".into(),
            "gpt-5.6-luna".into(),
            "openai-codex-responses".into(),
            true,
            false,
            None,
            "https://example.invalid".into(),
            HashMap::new(),
            "max".into(),
            zork_agent::session::ports::ModelLimits {
                context_window_tokens: 256_000,
                max_output_tokens: 32_000,
                reserve_percent: 10,
            },
            "secret".into(),
        );
        let request = ModelRequest {
            session_id: "session".into(),
            generation: 1,
            step_id: "step".into(),
            selection: zork_agent::session::wire::SessionSelection {
                profile_id: "profile".into(),
                model: "gpt-5.6-luna".into(),
                thinking: "max".into(),
            },
            transcript: Arc::new(Vec::new()),
            tools: Arc::new(Vec::new()),
            max_output_tokens: Some(32_000),
            independent: false,
            stream_observer: Arc::new(zork_agent::session::model::SilentStreamObserver),
        };

        let body = build_request(&request, &execution).unwrap().body;
        assert!(!body.contains_key("max_output_tokens"));
        assert_eq!(body.get("parallel_tool_calls"), Some(&json!(false)));

        let parallel_execution = ProfileExecution::new(
            "profile".into(),
            "openai".into(),
            "gpt-5.6-luna".into(),
            "openai-codex-responses".into(),
            true,
            true,
            None,
            "https://example.invalid".into(),
            HashMap::new(),
            "max".into(),
            zork_agent::session::ports::ModelLimits {
                context_window_tokens: 256_000,
                max_output_tokens: 32_000,
                reserve_percent: 10,
            },
            "secret".into(),
        );
        let body = build_request(&request, &parallel_execution).unwrap().body;
        assert_eq!(body.get("parallel_tool_calls"), Some(&json!(true)));
    }

    #[tokio::test]
    // Contract: docs/zork-agent-architecture.md [PROVIDER-01]
    async fn proxy_tunnel_uses_the_selected_authority_and_proxy_credentials() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut chunk = [0_u8; 256];
            loop {
                let read = stream.read(&mut chunk).await.unwrap();
                request.extend_from_slice(&chunk[..read]);
                if request.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            stream
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .await
                .unwrap();
            String::from_utf8(request).unwrap()
        });

        let mut client = TcpStream::connect(address).await.unwrap();
        establish_proxy_tunnel(&mut client, "codex.example:443", Some("Basic dXNlcjpwYXNz"))
            .await
            .unwrap();
        assert_eq!(
            server.await.unwrap(),
            "CONNECT codex.example:443 HTTP/1.1\r\nHost: codex.example:443\r\nProxy-Connection: Keep-Alive\r\nProxy-Authorization: Basic dXNlcjpwYXNz\r\n\r\n"
        );

        let proxy = proxy_from_values(
            "wss://codex.example/backend-api/codex/responses",
            Some("http://user:pass@proxy.example:8080"),
            None,
            None,
        )
        .unwrap()
        .unwrap();
        assert_eq!(proxy.host, "proxy.example");
        assert_eq!(proxy.port, 8080);
        assert_eq!(proxy.authorization.as_deref(), Some("Basic dXNlcjpwYXNz"));
    }

    /// 入帧空闲超时：服务端接受连接后装死（复刻 v2cover2 事故——请求在途
    /// 15 分钟无帧、无错误、无重试），短窗下必须以 retryable 错误浮出，
    /// 交给 step 内退避重试。
    #[tokio::test]
    // Contract: docs/zork-agent-architecture.md [PROVIDER-01, RETRY-01]
    async fn idle_timeout_breaks_silent_websocket() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            use futures_util::StreamExt;
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            // 收下请求，然后一个字都不发。
            let _ = ws.next().await;
            std::future::pending::<()>().await;
        });
        let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (ws, _response) = tokio_tungstenite::client_async_tls("ws://127.0.0.1/", tcp)
            .await
            .unwrap();
        let mut socket = CodexSocket::new(ws);
        socket
            .send(Message::Text(r#"{"type":"response.create"}"#.into()))
            .await
            .unwrap();

        let request = ModelRequest {
            session_id: "s".into(),
            generation: 1,
            step_id: "r".into(),
            selection: zork_agent::session::wire::SessionSelection {
                profile_id: "p".into(),
                model: "m".into(),
                thinking: "max".into(),
            },
            transcript: std::sync::Arc::new(Vec::new()),
            tools: std::sync::Arc::new(Vec::new()),
            max_output_tokens: None,
            independent: false,
            stream_observer: std::sync::Arc::new(zork_agent::session::model::SilentStreamObserver),
        };
        let started = std::time::Instant::now();
        let result = read_websocket_response_with_idle(
            &mut socket,
            &request,
            std::time::Duration::from_millis(200),
        )
        .await;
        match result {
            Err(ModelError::ProviderFailed(failure)) => {
                assert_eq!(failure.stage, "codex.websocket.idle");
                assert!(failure.retryable, "空闲超时必须可重试（接退避）");
            }
            _ => panic!("expected idle failure"),
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "空闲窗口应在秒级生效"
        );
        server.abort();
    }

    #[test]
    // Contract: docs/zork-agent-architecture.md [PROVIDER-01, RETRY-02]
    fn provider_error_event_preserves_explicit_status_and_code() {
        let error = provider_event_failure(&json!({
            "type": "error",
            "request_id": "req_123",
            "error": {
                "status": 429,
                "code": "rate_limit_exceeded",
                "message": "slow down"
            }
        }));
        let ModelError::ProviderFailed(failure) = error else {
            panic!("expected provider failure");
        };
        assert_eq!(failure.stage, "codex.websocket.provider_event");
        assert!(failure.retryable);
        assert_eq!(failure.status_code, Some(429));
        assert_eq!(failure.request_id, Some("req_123".to_owned()));
        assert_eq!(
            failure.provider_code,
            Some("rate_limit_exceeded".to_owned())
        );
        assert_eq!(failure.message, "slow down");
    }

    #[test]
    // Contract: docs/zork-agent-architecture.md [PROVIDER-01, RETRY-02]
    fn incomplete_event_preserves_its_reason() {
        let error = provider_event_failure(&json!({
            "type": "response.incomplete",
            "response": {
                "status": "incomplete",
                "incomplete_details": { "reason": "max_output_tokens" },
                "usage": {
                    "input_tokens": 321,
                    "input_tokens_details": { "cached_tokens": 123 },
                    "output_tokens": 45,
                    "output_tokens_details": { "reasoning_tokens": 34 }
                }
            }
        }));
        let ModelError::ProviderFailed(failure) = error else {
            panic!("expected provider failure");
        };
        assert_eq!(failure.provider_code.as_deref(), Some("max_output_tokens"));
        assert_eq!(
            failure.message,
            "provider response incomplete: max_output_tokens"
        );
        let usage = failure.usage.expect("incomplete response usage");
        assert_eq!(usage.input_tokens, 321);
        assert_eq!(usage.cached_input_tokens, Some(123));
        assert_eq!(usage.output_tokens, 45);
        assert_eq!(usage.output_reasoning_tokens, Some(34));
        assert!(failure.retryable);
    }

    /// 单连接 60 分钟硬限必须可重试：重试路径 session.reset() 已弃旧连接，
    /// 重连即获新窗口（v2idle 第 61 分钟非重试终结实证）。
    #[test]
    // Contract: docs/zork-agent-architecture.md [PROVIDER-01, RETRY-02]
    fn websocket_connection_limit_is_retryable() {
        let error = provider_event_failure(&json!({
            "type": "error",
            "error": {
                "code": "websocket_connection_limit_reached",
                "message": "Responses websocket connection limit reached (60 minutes). Create a new websocket connection to continue."
            }
        }));
        let ModelError::ProviderFailed(failure) = error else {
            panic!("expected provider failure");
        };
        assert_eq!(
            failure.provider_code.as_deref(),
            Some("websocket_connection_limit_reached")
        );
        assert!(failure.retryable, "连接限须可重试（重连即愈）");
    }
}
