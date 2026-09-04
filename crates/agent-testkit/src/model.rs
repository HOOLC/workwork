use std::pin::Pin;
use std::sync::{Arc, Mutex};

use serde_json::Value;
use zork_agent::session::model::{
    ModelError, ModelGateway, ModelOutcome, ModelReleaseSuggestion, ModelRequest,
    ModelStreamObserver, ModelTokenUsage, ProviderFailure, ToolDefinition,
};
use zork_agent::session::tools::PROVIDER_CALL_NAME;
use zork_agent::session::wire::{ProviderMessage, ProviderToolCall, SessionSelection};

pub struct ControlledModel {
    requests: tokio::sync::mpsc::Receiver<CapturedRequest>,
    releases: Arc<Mutex<Vec<ModelRelease>>>,
}

pub struct ControlledModelGateway {
    requests: tokio::sync::mpsc::Sender<CapturedRequest>,
    releases: Arc<Mutex<Vec<ModelRelease>>>,
}

struct CapturedRequest {
    request: PendingModelRequest,
}

pub struct PendingModelRequest {
    pub session_id: String,
    pub generation: u64,
    pub step_id: String,
    pub selection: SessionSelection,
    pub transcript: Arc<Vec<ProviderMessage>>,
    pub tools: Arc<Vec<ToolDefinition>>,
    pub max_output_tokens: Option<u32>,
    pub independent: bool,
    stream_observer: Arc<dyn ModelStreamObserver>,
    response: tokio::sync::oneshot::Sender<ControlledReply>,
}

enum ControlledReply {
    Outcome(Result<ModelOutcome, ModelError>),
    Panic(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelRelease {
    Session(String),
    Generation { session_id: String, generation: u64 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("the model request is no longer waiting for a response")]
pub struct ModelRequestClosed;

impl ControlledModel {
    pub fn pair(capacity: usize) -> (Arc<ControlledModelGateway>, Self) {
        let (sender, requests) = tokio::sync::mpsc::channel(capacity.max(1));
        let releases = Arc::new(Mutex::new(Vec::new()));
        (
            Arc::new(ControlledModelGateway {
                requests: sender,
                releases: releases.clone(),
            }),
            Self { requests, releases },
        )
    }

    pub async fn request(&mut self) -> PendingModelRequest {
        tokio::time::timeout(std::time::Duration::from_secs(10), self.requests.recv())
            .await
            .expect("timed out waiting for the expected model request")
            .expect("zork-agent stopped before issuing the expected model request")
            .request
    }

    pub fn releases(&self) -> Vec<ModelRelease> {
        self.releases
            .lock()
            .expect("controlled model release lock poisoned")
            .clone()
    }
}

impl PendingModelRequest {
    pub fn stream_text(&self, text: &str) {
        self.stream_observer
            .text_delta(&self.session_id, self.generation, &self.step_id, text);
    }

    pub fn respond(
        self,
        outcome: Result<ModelOutcome, ModelError>,
    ) -> Result<(), ModelRequestClosed> {
        self.response
            .send(ControlledReply::Outcome(outcome))
            .map_err(|_| ModelRequestClosed)
    }

    pub fn panic_model_task(self, message: impl Into<String>) -> Result<(), ModelRequestClosed> {
        self.response
            .send(ControlledReply::Panic(message.into()))
            .map_err(|_| ModelRequestClosed)
    }

    pub fn respond_text(self, text: impl Into<String>) -> Result<(), ModelRequestClosed> {
        self.respond(Ok(successful_outcome(text.into(), Vec::new())))
    }

    pub fn fail_provider(
        self,
        stage: &'static str,
        retryable: bool,
        message: impl Into<String>,
    ) -> Result<(), ModelRequestClosed> {
        self.respond(Err(ModelError::ProviderFailed(ProviderFailure::new(
            stage, retryable, message,
        ))))
    }

    pub fn respond_call(
        self,
        provider_call_id: impl Into<String>,
        tool: impl Into<String>,
        arguments: Value,
    ) -> Result<(), ModelRequestClosed> {
        self.respond_calls([(provider_call_id, tool, arguments)])
    }

    pub fn respond_calls<I, C, T>(self, calls: I) -> Result<(), ModelRequestClosed>
    where
        I: IntoIterator<Item = (C, T, Value)>,
        C: Into<String>,
        T: Into<String>,
    {
        self.respond(Ok(successful_outcome(
            String::new(),
            calls
                .into_iter()
                .map(|(provider_call_id, tool, arguments)| ProviderToolCall {
                    tool_call_id: provider_call_id.into(),
                    tool_name: PROVIDER_CALL_NAME.into(),
                    arguments: serde_json::json!({
                        "tool": tool.into(),
                        "arguments": arguments,
                    }),
                })
                .collect(),
        )))
    }
}

impl ModelGateway for ControlledModelGateway {
    fn complete<'a>(
        &'a self,
        request: &'a ModelRequest,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<ModelOutcome, ModelError>> + Send + 'a>>
    {
        let (response, received) = tokio::sync::oneshot::channel();
        let pending = PendingModelRequest {
            session_id: request.session_id.clone(),
            generation: request.generation,
            step_id: request.step_id.clone(),
            selection: request.selection.clone(),
            transcript: request.transcript.clone(),
            tools: request.tools.clone(),
            max_output_tokens: request.max_output_tokens,
            independent: request.independent,
            stream_observer: request.stream_observer.clone(),
            response,
        };
        let requests = self.requests.clone();
        Box::pin(async move {
            requests
                .send(CapturedRequest { request: pending })
                .await
                .map_err(|_| ModelError::Unavailable)?;
            match received.await {
                Ok(ControlledReply::Outcome(outcome)) => outcome,
                Ok(ControlledReply::Panic(message)) => panic!("{message}"),
                Err(_) => Err(ModelError::Unavailable),
            }
        })
    }

    fn release(&self, suggestion: ModelReleaseSuggestion<'_>) {
        let release = match suggestion {
            ModelReleaseSuggestion::Session(session_id) => {
                ModelRelease::Session(session_id.to_owned())
            }
            ModelReleaseSuggestion::Generation {
                session_id,
                generation,
            } => ModelRelease::Generation {
                session_id: session_id.to_owned(),
                generation,
            },
        };
        self.releases
            .lock()
            .expect("controlled model release lock poisoned")
            .push(release);
    }
}

fn successful_outcome(text: String, tool_calls: Vec<ProviderToolCall>) -> ModelOutcome {
    ModelOutcome {
        text,
        tool_calls,
        provider_context: None,
        usage: Some(ModelTokenUsage {
            input_tokens: 1,
            cached_input_tokens: None,
            output_tokens: 1,
            output_reasoning_tokens: None,
            output_text_tokens: Some(1),
        }),
        provider_input: None,
    }
}
