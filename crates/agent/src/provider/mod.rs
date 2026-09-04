mod agent_port;
mod codex_responses;
mod fake;
mod responses;

pub use agent_port::AgentModelPort;
pub use fake::FakeProvider;

use aimux_core::{
    content::ContentPart,
    error::AiMuxError,
    language_model::LanguageModel,
    language_model_message::{LanguageModelPrompt, LanguageModelPromptMessage},
    message::Role,
    options::CallOptions,
    result::{GenerateContent, GenerateResult, StreamResult},
    stream_part::StreamPart,
    tool::{FunctionTool, Tool},
    types::{FinishReason, FinishReasonUnified, ReasoningEffort, Usage},
};
use aimux_providers::{
    anthropic::{AnthropicConfig, AnthropicProvider},
    openai::{OpenAIConfig, OpenAIProvider},
};
use futures_util::StreamExt;
use serde_json::Value;
use std::collections::HashMap;
use url::Url;

use zork_agent::session::{
    model::{ModelError, ModelOutcome, ModelRequest, ProviderFailure},
    ports::{ModelExecutor, ProfileExecution},
    wire::{
        ProviderContext, ProviderInputDiagnostics, ProviderInputMode, ProviderMessage,
        ProviderToolCall, TranscriptRole,
    },
};

pub struct ProviderRouter {
    codex_responses: codex_responses::CodexResponsesProvider,
}

impl Default for ProviderRouter {
    fn default() -> Self {
        Self::new()
    }
}

fn aimux_failure(stage: &'static str, error: AiMuxError) -> ModelError {
    let status_code = error.status_code();
    let (provider_code, request_id) = match &error {
        AiMuxError::ApiCall(detail) => (detail.provider_code.clone(), detail.request_id.clone()),
        _ => (None, None),
    };
    let retryable = !provider_failure_is_certainly_permanent(status_code, provider_code.as_deref());
    ModelError::ProviderFailed(ProviderFailure {
        stage,
        retryable,
        status_code,
        provider_code,
        request_id,
        message: error.to_string(),
        provider_input: None,
        usage: None,
    })
}

fn protocol_failure(stage: &'static str, message: impl Into<String>) -> ModelError {
    ModelError::ProviderFailed(ProviderFailure::new(stage, true, message))
}

pub(super) fn provider_failure_is_certainly_permanent(
    status_code: Option<u16>,
    provider_code: Option<&str>,
) -> bool {
    if matches!(status_code, Some(401 | 403)) {
        return true;
    }
    let Some(code) = provider_code else {
        return false;
    };
    matches!(
        code.to_ascii_lowercase().as_str(),
        "invalid_api_key"
            | "authentication_error"
            | "unauthorized"
            | "permission_denied"
            | "account_deactivated"
    )
}

impl ProviderRouter {
    pub fn new() -> Self {
        Self {
            codex_responses: codex_responses::CodexResponsesProvider::new(),
        }
    }

    async fn complete_request(
        &self,
        request: &ModelRequest,
        execution: ProfileExecution,
    ) -> Result<ModelOutcome, ModelError> {
        let selection = &request.selection;
        if execution.profile_id() != selection.profile_id
            || execution.model() != selection.model
            || execution.thinking() != selection.thinking
        {
            return Err(ModelError::InvalidSelection);
        }
        let base_url =
            Url::parse(execution.base_url()).map_err(|_| ModelError::InvalidSelection)?;
        if !matches!(base_url.scheme(), "http" | "https") {
            return Err(ModelError::InvalidSelection);
        }
        if execution.api() == "openai-codex-responses" {
            return self.codex_responses.complete(request, execution).await;
        }

        let responses_api = is_responses_api(execution.api());
        let prompt = if responses_api {
            Vec::new()
        } else {
            prompt_from_transcript(request.transcript.as_slice(), &execution)?
        };
        let tools = request
            .tools
            .iter()
            .map(|tool| {
                Tool::Function(
                    FunctionTool::new(tool.name.clone(), tool.input_schema.clone())
                        .with_description(tool.description.clone()),
                )
            })
            .collect::<Vec<_>>();
        let options = CallOptions {
            tools: (!tools.is_empty()).then_some(tools),
            max_output_tokens: request.max_output_tokens,
            headers: (!execution.headers().is_empty()).then(|| execution.headers().clone()),
            reasoning: Some(reasoning_effort(execution.thinking())),
            provider_options: responses_api
                .then(|| responses_provider_options(execution.thinking())),
            ..CallOptions::new(prompt)
        };
        if responses_api {
            let input = responses_input_from_transcript(request.transcript.as_slice(), &execution)?;
            let provider_input = full_input_diagnostics(request.transcript.len(), input.len());
            if execution.streaming() {
                let result = responses::do_stream(&options, &execution, input)
                    .await
                    .map_err(|error| {
                        with_provider_diagnostics(
                            aimux_failure("openai.responses.stream_start", error),
                            Some(Box::new(provider_input.clone())),
                            None,
                        )
                    })?;
                return model_outcome_from_stream_result(
                    result.result,
                    request,
                    &execution,
                    Some(result.output_items),
                    provider_input,
                )
                .await;
            }
            let result = responses::do_generate(&options, &execution, input)
                .await
                .map_err(|error| {
                    with_provider_diagnostics(
                        aimux_failure("openai.responses.generate", error),
                        Some(Box::new(provider_input.clone())),
                        None,
                    )
                })?;
            return model_outcome_from_generate_result(
                result.result,
                &execution,
                Some(result.output_items),
                provider_input,
            );
        }
        let provider_input = full_input_diagnostics(request.transcript.len(), options.prompt.len());
        if execution.streaming() {
            let result = stream_request(&options, &execution)
                .await
                .map_err(|error| {
                    with_provider_diagnostics(error, Some(Box::new(provider_input.clone())), None)
                })?;
            model_outcome_from_stream_result(result, request, &execution, None, provider_input)
                .await
        } else {
            let result = generate_request(&options, &execution)
                .await
                .map_err(|error| {
                    with_provider_diagnostics(error, Some(Box::new(provider_input.clone())), None)
                })?;
            model_outcome_from_generate_result(result, &execution, None, provider_input)
        }
    }
}

fn full_input_diagnostics(logical_items: usize, sent_items: usize) -> ProviderInputDiagnostics {
    ProviderInputDiagnostics {
        mode: ProviderInputMode::Full,
        logical_input_items: u64::try_from(logical_items).unwrap_or(u64::MAX),
        sent_input_items: u64::try_from(sent_items).unwrap_or(u64::MAX),
        previous_response_id: None,
        response_id: None,
    }
}

fn with_provider_diagnostics(
    mut error: ModelError,
    provider_input: Option<Box<ProviderInputDiagnostics>>,
    usage: Option<zork_agent::session::model::ModelTokenUsage>,
) -> ModelError {
    if let ModelError::ProviderFailed(failure) = &mut error {
        failure.provider_input = provider_input;
        failure.usage = usage.map(Box::new);
    }
    error
}

async fn stream_request(
    options: &CallOptions,
    execution: &ProfileExecution,
) -> Result<StreamResult, ModelError> {
    match execution.api() {
        "openai-completions" => {
            let mut config = OpenAIConfig::new(execution.secret().to_owned())
                .with_base_url(execution.base_url().to_owned())
                .with_provider(execution.provider().to_owned())
                .with_headers(execution.headers().clone());
            config.retry_config.max_retries = 0;
            OpenAIProvider::new(config)
                .model(execution.model())
                .do_stream(options)
                .await
                .map_err(|error| aimux_failure("openai.completions.stream_start", error))
        }
        "anthropic-messages" => {
            let mut config = AnthropicConfig::new(execution.secret().to_owned())
                .with_base_url(execution.base_url().to_owned())
                .with_headers(execution.headers().clone());
            config.retry_config.max_retries = 0;
            AnthropicProvider::new(config)
                .model(execution.model())
                .do_stream(options)
                .await
                .map_err(|error| aimux_failure("anthropic.messages.stream_start", error))
        }
        _ => Err(ModelError::InvalidSelection),
    }
}

async fn generate_request(
    options: &CallOptions,
    execution: &ProfileExecution,
) -> Result<GenerateResult, ModelError> {
    match execution.api() {
        "openai-completions" => {
            let mut config = OpenAIConfig::new(execution.secret().to_owned())
                .with_base_url(execution.base_url().to_owned())
                .with_provider(execution.provider().to_owned())
                .with_headers(execution.headers().clone());
            config.retry_config.max_retries = 0;
            OpenAIProvider::new(config)
                .model(execution.model())
                .do_generate(options)
                .await
                .map_err(|error| aimux_failure("openai.completions.generate", error))
        }
        "anthropic-messages" => {
            let mut config = AnthropicConfig::new(execution.secret().to_owned())
                .with_base_url(execution.base_url().to_owned())
                .with_headers(execution.headers().clone());
            config.retry_config.max_retries = 0;
            AnthropicProvider::new(config)
                .model(execution.model())
                .do_generate(options)
                .await
                .map_err(|error| aimux_failure("anthropic.messages.generate", error))
        }
        _ => Err(ModelError::InvalidSelection),
    }
}

struct PendingToolCall {
    tool_call_id: String,
    tool_name: String,
    input: String,
    parsed: Option<Value>,
}

#[derive(Default)]
struct ModelOutcomeAccumulator {
    text: String,
    tool_calls: Vec<PendingToolCall>,
    provider_context: Option<ProviderContext>,
    usage: Option<zork_agent::session::model::ModelTokenUsage>,
    provider_input: Option<Box<ProviderInputDiagnostics>>,
}

impl ModelOutcomeAccumulator {
    fn tool_call_mut(&mut self, tool_call_id: &str) -> Option<&mut PendingToolCall> {
        self.tool_calls
            .iter_mut()
            .find(|call| call.tool_call_id == tool_call_id)
    }

    fn attach_failure(&self, error: ModelError) -> ModelError {
        with_provider_diagnostics(error, self.provider_input.clone(), self.usage.clone())
    }

    fn observe_response_id(&mut self, response_id: Option<String>) {
        if let (Some(input), Some(response_id)) = (&mut self.provider_input, response_id) {
            input.response_id = Some(response_id);
        }
    }

    fn complete(self, finish_reason: FinishReason) -> Result<ModelOutcome, ModelError> {
        let Self {
            text,
            tool_calls,
            provider_context,
            usage,
            provider_input,
        } = self;
        let failure = |stage, message| {
            with_provider_diagnostics(
                protocol_failure(stage, message),
                provider_input.clone(),
                usage.clone(),
            )
        };
        match finish_reason.unified {
            FinishReasonUnified::Stop if tool_calls.is_empty() => Ok(ModelOutcome {
                text,
                tool_calls: Vec::new(),
                provider_context,
                usage,
                provider_input,
            }),
            FinishReasonUnified::ToolCalls if !tool_calls.is_empty() => {
                let mut calls = Vec::with_capacity(tool_calls.len());
                for call in tool_calls {
                    let input = match call.parsed {
                        Some(Value::String(raw)) => {
                            serde_json::from_str(&raw).map_err(|error| {
                                failure("provider.outcome.tool_arguments_json", error.to_string())
                            })?
                        }
                        Some(input) => input,
                        None => serde_json::from_str(&call.input).map_err(|error| {
                            failure("provider.outcome.tool_arguments_json", error.to_string())
                        })?,
                    };
                    if !input.is_object() {
                        return Err(failure(
                            "provider.outcome.tool_arguments_shape",
                            "tool arguments are not an object".to_owned(),
                        ));
                    }
                    calls.push(ProviderToolCall {
                        tool_call_id: call.tool_call_id,
                        tool_name: call.tool_name,
                        arguments: input,
                    });
                }
                Ok(ModelOutcome {
                    text,
                    tool_calls: calls,
                    provider_context,
                    usage,
                    provider_input,
                })
            }
            FinishReasonUnified::Length | FinishReasonUnified::ContentFilter => {
                let mut error = failure(
                    "provider.outcome.finish_reason",
                    format!("unsupported finish reason: {finish_reason:?}"),
                );
                if let ModelError::ProviderFailed(detail) = &mut error {
                    detail.provider_code = Some(
                        if finish_reason.unified == FinishReasonUnified::Length {
                            "max_output_tokens"
                        } else {
                            "content_filter"
                        }
                        .into(),
                    );
                }
                Err(error)
            }
            FinishReasonUnified::Stop
            | FinishReasonUnified::ToolCalls
            | FinishReasonUnified::Error
            | FinishReasonUnified::Other => Err(failure(
                "provider.outcome.finish_reason",
                format!("unsupported finish reason: {:?}", finish_reason),
            )),
        }
    }
}

async fn model_outcome_from_stream_result(
    mut result: StreamResult,
    request: &ModelRequest,
    execution: &ProfileExecution,
    output_items: Option<responses::CapturedOutputItems>,
    provider_input: ProviderInputDiagnostics,
) -> Result<ModelOutcome, ModelError> {
    let mut outcome = ModelOutcomeAccumulator {
        provider_input: Some(Box::new(provider_input)),
        ..Default::default()
    };
    let mut finish_reason = None;
    while let Some(part) = result.stream.next().await {
        let part = match part {
            Ok(part) => part,
            Err(error) => {
                return Err(outcome.attach_failure(aimux_failure("provider.stream.read", error)))
            }
        };
        match part {
            StreamPart::TextDelta { delta, .. } => {
                request.stream_observer.text_delta(
                    &request.session_id,
                    request.generation,
                    &request.step_id,
                    &delta,
                );
                outcome.text.push_str(&delta);
            }
            StreamPart::ToolInputStart { id, tool_name, .. }
                if outcome.tool_call_mut(&id).is_none() =>
            {
                outcome.tool_calls.push(PendingToolCall {
                    tool_call_id: id,
                    tool_name,
                    input: String::new(),
                    parsed: None,
                });
            }
            StreamPart::ToolInputDelta { id, delta, .. } => {
                let Some(call) = outcome.tool_call_mut(&id) else {
                    return Err(outcome.attach_failure(protocol_failure(
                        "provider.stream.tool_input_delta",
                        "tool input delta has no matching call",
                    )));
                };
                call.input.push_str(&delta);
            }
            StreamPart::ToolInputEnd { id, .. } if outcome.tool_call_mut(&id).is_none() => {
                return Err(outcome.attach_failure(protocol_failure(
                    "provider.stream.tool_input_end",
                    "tool input end has no matching call",
                )));
            }
            StreamPart::ToolInputEnd { .. } => {}
            StreamPart::ToolCall {
                tool_call_id,
                tool_name,
                input,
                ..
            } => {
                if let Some(call) = outcome.tool_call_mut(&tool_call_id) {
                    call.tool_name = tool_name;
                    match input {
                        Value::String(raw) => {
                            call.input = raw;
                            call.parsed = None;
                        }
                        input => call.parsed = Some(input),
                    }
                } else {
                    let (input, parsed) = match input {
                        Value::String(raw) => (raw, None),
                        input => (String::new(), Some(input)),
                    };
                    outcome.tool_calls.push(PendingToolCall {
                        tool_call_id,
                        tool_name,
                        input,
                        parsed,
                    });
                }
            }
            StreamPart::ResponseMetadata { id, .. } => outcome.observe_response_id(id),
            StreamPart::ReasoningStart { .. } | StreamPart::ReasoningEnd { .. } => {}
            StreamPart::Finish {
                finish_reason: observed_finish,
                usage,
                ..
            } => {
                outcome.usage = model_token_usage(&usage);
                finish_reason = Some(observed_finish);
            }
            StreamPart::Error { error } => {
                return Err(
                    outcome.attach_failure(aimux_failure("provider.stream.error_event", error))
                )
            }
            _ => {}
        }
    }
    if let Some(output_items) = output_items {
        if !output_items.terminal_received() {
            return Err(outcome.attach_failure(protocol_failure(
                "provider.stream.finish",
                "Responses stream ended without a terminal response event",
            )));
        }
        let ordered = output_items.ordered().map_err(|error| {
            outcome.attach_failure(protocol_failure("provider.output_items", error))
        })?;
        outcome.provider_context = Some(provider_context_from_output_items(ordered, execution));
    }
    let Some(finish_reason) = finish_reason else {
        return Err(outcome.attach_failure(protocol_failure(
            "provider.stream.finish",
            "provider stream ended without a finish event",
        )));
    };
    if execution.api() == "openai-completions" && finish_reason.raw.is_none() {
        return Err(outcome.attach_failure(protocol_failure(
            "provider.stream.finish",
            "provider stream ended without an explicit finish reason",
        )));
    }
    outcome.complete(finish_reason)
}

fn model_outcome_from_generate_result(
    result: GenerateResult,
    execution: &ProfileExecution,
    output_items: Option<std::sync::Arc<Vec<Value>>>,
    provider_input: ProviderInputDiagnostics,
) -> Result<ModelOutcome, ModelError> {
    let mut outcome = ModelOutcomeAccumulator {
        usage: model_token_usage(&result.usage),
        provider_input: Some(Box::new(provider_input)),
        ..Default::default()
    };
    outcome.observe_response_id(result.response.id.clone());
    for content in result.content {
        match content {
            GenerateContent::Text { text, .. } => outcome.text.push_str(&text),
            GenerateContent::ToolCall {
                tool_call_id,
                tool_name,
                input,
                ..
            } => outcome.tool_calls.push(PendingToolCall {
                tool_call_id,
                tool_name,
                input: String::new(),
                parsed: Some(input),
            }),
            GenerateContent::Reasoning { .. } => {}
            GenerateContent::Source { .. }
            | GenerateContent::File { .. }
            | GenerateContent::ToolResult { .. } => {}
        }
    }
    if let Some(output_items) = output_items {
        outcome.provider_context =
            Some(provider_context_from_output_items(output_items, execution));
    }
    outcome.complete(result.finish_reason)
}

fn provider_context_from_output_items(
    output_items: std::sync::Arc<Vec<Value>>,
    execution: &ProfileExecution,
) -> ProviderContext {
    ProviderContext {
        profile_id: execution.profile_id().to_owned(),
        provider: execution.provider().to_owned(),
        model: execution.model().to_owned(),
        api: execution.api().to_owned(),
        output_items,
    }
}

fn model_token_usage(usage: &Usage) -> Option<zork_agent::session::model::ModelTokenUsage> {
    usage
        .input_tokens
        .total
        .map(|input_tokens| zork_agent::session::model::ModelTokenUsage {
            input_tokens: u64::from(input_tokens),
            cached_input_tokens: usage.input_tokens.cache_read.map(u64::from),
            output_tokens: u64::from(usage.output_tokens.total.unwrap_or(0)),
            output_reasoning_tokens: usage.output_tokens.reasoning.map(u64::from),
            output_text_tokens: usage.output_tokens.text.map(u64::from),
        })
}

fn reasoning_effort(value: &str) -> ReasoningEffort {
    match value {
        "none" | "off" => ReasoningEffort::None,
        "minimal" => ReasoningEffort::Minimal,
        "low" => ReasoningEffort::Low,
        "medium" => ReasoningEffort::Medium,
        "high" => ReasoningEffort::High,
        "xhigh" => ReasoningEffort::Xhigh,
        _ => ReasoningEffort::ProviderDefault,
    }
}

fn responses_provider_options(thinking: &str) -> HashMap<String, Value> {
    HashMap::from([(
        "openai".to_owned(),
        serde_json::json!({
            "forceReasoning": thinking != "off",
            "reasoningEffort": if thinking == "off" { "none" } else { thinking },
            "store": false,
        }),
    )])
}

fn is_responses_api(api: &str) -> bool {
    api == "openai-responses"
}

impl ModelExecutor for ProviderRouter {
    fn complete<'a>(
        &'a self,
        request: &'a ModelRequest,
        execution: ProfileExecution,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<ModelOutcome, ModelError>> + Send + 'a>,
    > {
        Box::pin(self.complete_request(request, execution))
    }

    fn release_session(&self, session_id: &str) {
        self.codex_responses.release_session(session_id);
    }
}

fn responses_input_from_transcript(
    transcript: &[ProviderMessage],
    execution: &ProfileExecution,
) -> Result<Vec<Value>, ModelError> {
    let mut input = Vec::new();
    for message in transcript {
        match message.role {
            TranscriptRole::System => input.push(serde_json::json!({
                "role": "developer",
                "content": message.content,
            })),
            TranscriptRole::User => input.push(serde_json::json!({
                "role": "user",
                "content": [{ "type": "input_text", "text": message.content }],
            })),
            TranscriptRole::Assistant => {
                let matching_context = message.provider_context.as_ref().filter(|context| {
                    context.profile_id == execution.profile_id()
                        && context.provider == execution.provider()
                        && context.model == execution.model()
                        && context.api == execution.api()
                });
                if let Some(context) = matching_context {
                    input.extend(context.output_items.iter().cloned());
                    continue;
                }
                if !message.content.is_empty() || message.tool_calls.is_empty() {
                    input.push(serde_json::json!({
                        "role": "assistant",
                        "content": [{ "type": "output_text", "text": message.content }],
                    }));
                }
                for call in &message.tool_calls {
                    input.push(serde_json::json!({
                        "type": "function_call",
                        "call_id": call.tool_call_id,
                        "name": call.tool_name,
                        "arguments": serde_json::to_string(&call.arguments).map_err(|error| {
                            protocol_failure(
                                "provider.prompt.tool_arguments_json",
                                error.to_string(),
                            )
                        })?,
                    }));
                }
            }
            TranscriptRole::Tool => {
                let tool_call_id = message.tool_call_id.as_deref().ok_or_else(|| {
                    protocol_failure(
                        "provider.prompt.tool_result_call_id",
                        "tool result has no tool call id",
                    )
                })?;
                input.push(serde_json::json!({
                    "type": "function_call_output",
                    "call_id": tool_call_id,
                    "output": message.content,
                }));
            }
        }
    }
    Ok(input)
}

fn prompt_from_transcript(
    transcript: &[ProviderMessage],
    execution: &ProfileExecution,
) -> Result<LanguageModelPrompt, ModelError> {
    transcript
        .iter()
        .map(|message| {
            let role = match message.role {
                TranscriptRole::System => Role::System,
                TranscriptRole::User => Role::User,
                TranscriptRole::Assistant => Role::Assistant,
                TranscriptRole::Tool => Role::Tool,
            };
            let mut content = Vec::new();
            // The durable transcript keeps any assistant preamble alongside
            // its tool calls, but OpenAI-compatible tool-call turns use a
            // null/omitted content field on the wire. Projecting that text
            // back into the next request changes the provider conversation
            // (and breaks replay) even though the text remains observable in
            // the durable Agent transcript.
            let assistant_tool_turn = execution.api() == "openai-completions"
                && message.role == TranscriptRole::Assistant
                && !message.tool_calls.is_empty();
            if message.role != TranscriptRole::Tool
                && !message.content.is_empty()
                && !assistant_tool_turn
            {
                content.push(ContentPart::text(message.content.to_string()));
            }
            if message.role == TranscriptRole::Assistant {
                for call in &message.tool_calls {
                    content.push(ContentPart::tool_call(
                        call.tool_call_id.clone(),
                        call.tool_name.clone(),
                        call.arguments.clone(),
                    ));
                }
            }
            if message.role == TranscriptRole::Tool {
                let tool_call_id = message.tool_call_id.clone().ok_or_else(|| {
                    protocol_failure(
                        "provider.prompt.tool_result_call_id",
                        "tool result has no tool call id",
                    )
                })?;
                let result = serde_json::from_str(&message.content)
                    .unwrap_or_else(|_| Value::String(message.content.to_string()));
                content.push(ContentPart::ToolResult {
                    tool_call_id,
                    result,
                    tool_name: None,
                    is_error: Some(message.is_error),
                    preliminary: None,
                    dynamic: None,
                    provider_options: None,
                });
            }
            if content.is_empty() {
                content.push(ContentPart::text(String::new()));
            }
            Ok(LanguageModelPromptMessage {
                role,
                content,
                provider_options: None,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    // Contract: docs/zork-agent-architecture.md [PROVIDER-01, RETRY-02]
    fn aimux_failure_preserves_structured_diagnostics_without_raw_body() {
        let error = AiMuxError::ApiCall(aimux_core::ApiCallError {
            status_code: Some(429),
            provider_code: Some("rate_limit_exceeded".to_owned()),
            message: "slow down".to_owned(),
            response_body: Some("raw body must not be copied".to_owned()),
            request_id: Some("req_123".to_owned()),
            is_retryable: true,
            ..Default::default()
        });

        let ModelError::ProviderFailed(failure) =
            aimux_failure("openai.responses.stream_start", error)
        else {
            panic!("expected provider failure");
        };
        assert_eq!(failure.stage, "openai.responses.stream_start");
        assert!(failure.retryable);
        assert_eq!(failure.status_code, Some(429));
        assert_eq!(
            failure.provider_code.as_deref(),
            Some("rate_limit_exceeded")
        );
        assert_eq!(failure.request_id.as_deref(), Some("req_123"));
        assert!(failure.message.contains("slow down"));
        assert!(!failure.message.contains("raw body must not be copied"));
    }

    fn execution(api: &str, streaming: bool) -> ProfileExecution {
        ProfileExecution::new(
            "profile".to_owned(),
            "openai".to_owned(),
            "gpt-5.6-luna".to_owned(),
            api.to_owned(),
            streaming,
            false,
            None,
            "https://example.invalid".to_owned(),
            HashMap::new(),
            "max".to_owned(),
            zork_agent::session::ports::ModelLimits {
                context_window_tokens: 1_000_000,
                max_output_tokens: 128_000,
                reserve_percent: 10,
            },
            "secret".to_owned(),
        )
    }

    fn stream_request_fixture() -> ModelRequest {
        ModelRequest {
            session_id: "session".to_owned(),
            generation: 0,
            step_id: "step".to_owned(),
            selection: zork_agent::session::wire::SessionSelection {
                profile_id: "profile".to_owned(),
                model: "gpt-5.6-luna".to_owned(),
                thinking: "max".to_owned(),
            },
            transcript: std::sync::Arc::new(Vec::new()),
            tools: std::sync::Arc::new(Vec::new()),
            max_output_tokens: Some(128_000),
            independent: false,
            stream_observer: std::sync::Arc::new(zork_agent::session::model::SilentStreamObserver),
        }
    }

    fn stream_result(parts: Vec<StreamPart>) -> StreamResult {
        StreamResult {
            stream: Box::pin(futures_util::stream::iter(
                parts.into_iter().map(Ok::<_, AiMuxError>),
            )),
            request_body: None,
            response_headers: None,
        }
    }

    fn usage(input: u32, output: u32) -> Usage {
        Usage {
            input_tokens: aimux_core::types::TokenUsage {
                total: Some(input),
                ..Default::default()
            },
            output_tokens: aimux_core::types::TokenUsage {
                total: Some(output),
                reasoning: Some(output),
                ..Default::default()
            },
            raw: None,
        }
    }

    #[tokio::test]
    // Contract: docs/zork-agent-architecture.md [PROVIDER-01, RETRY-02]
    async fn incomplete_tool_stream_keeps_request_and_usage_without_a_false_json_error() {
        let partial = r#"{"tool":"file.read","arguments":{"path":"/tmp"#;
        let result = stream_result(vec![
            StreamPart::ResponseMetadata {
                id: Some("response-123".to_owned()),
                timestamp: None,
                model_id: None,
            },
            StreamPart::ToolInputStart {
                id: "call-1".to_owned(),
                tool_name: "call".to_owned(),
                provider_executed: None,
                dynamic: None,
                title: None,
                provider_metadata: None,
            },
            StreamPart::ToolInputDelta {
                id: "call-1".to_owned(),
                delta: partial.to_owned(),
                provider_metadata: None,
            },
            StreamPart::ToolInputEnd {
                id: "call-1".to_owned(),
                provider_metadata: None,
            },
            StreamPart::ToolCall {
                tool_call_id: "call-1".to_owned(),
                tool_name: "call".to_owned(),
                input: Value::String(partial.to_owned()),
                provider_executed: None,
                dynamic: None,
                thought_signature: None,
                provider_metadata: None,
            },
            StreamPart::Finish {
                finish_reason: FinishReason {
                    unified: FinishReasonUnified::Stop,
                    raw: None,
                },
                usage: usage(321, 45),
                provider_metadata: None,
            },
        ]);

        let error = model_outcome_from_stream_result(
            result,
            &stream_request_fixture(),
            &execution("openai-completions", true),
            None,
            full_input_diagnostics(4, 5),
        )
        .await
        .expect_err("missing finish reason must fail");
        let ModelError::ProviderFailed(failure) = error else {
            panic!("expected provider failure");
        };
        assert_eq!(failure.stage, "provider.stream.finish");
        assert!(!failure.message.contains("EOF while parsing"));
        let input = failure.provider_input.expect("provider input");
        assert_eq!(input.logical_input_items, 4);
        assert_eq!(input.sent_input_items, 5);
        assert_eq!(input.response_id.as_deref(), Some("response-123"));
        let usage = failure.usage.expect("observed usage");
        assert_eq!(usage.input_tokens, 321);
        assert_eq!(usage.output_tokens, 45);
    }

    #[tokio::test]
    // Contract: docs/zork-agent-architecture.md [PROVIDER-01, RETRY-02]
    async fn transport_error_after_tool_input_end_is_not_masked_by_partial_json() {
        let result = stream_result(vec![
            StreamPart::ToolInputStart {
                id: "call-1".to_owned(),
                tool_name: "call".to_owned(),
                provider_executed: None,
                dynamic: None,
                title: None,
                provider_metadata: None,
            },
            StreamPart::ToolInputDelta {
                id: "call-1".to_owned(),
                delta: "{".to_owned(),
                provider_metadata: None,
            },
            StreamPart::ToolInputEnd {
                id: "call-1".to_owned(),
                provider_metadata: None,
            },
            StreamPart::Error {
                error: AiMuxError::InvalidResponseData("transport reset".to_owned()),
            },
        ]);

        let error = model_outcome_from_stream_result(
            result,
            &stream_request_fixture(),
            &execution("openai-completions", true),
            None,
            full_input_diagnostics(2, 2),
        )
        .await
        .expect_err("transport error must fail");
        let ModelError::ProviderFailed(failure) = error else {
            panic!("expected provider failure");
        };
        assert_eq!(failure.stage, "provider.stream.error_event");
        assert!(failure.message.contains("transport reset"));
        assert_eq!(failure.provider_input.unwrap().sent_input_items, 2);
    }

    #[test]
    // Contract: docs/zork-agent-architecture.md [PROVIDER-01]
    fn non_streaming_result_preserves_the_streaming_outcome_contract() {
        let result = aimux_core::result::GenerateResult {
            content: vec![
                aimux_core::result::GenerateContent::Reasoning {
                    text: "summary".to_owned(),
                    provider_metadata: Some(serde_json::json!({
                        "openai": {
                            "itemId": "rs_1",
                            "reasoningEncryptedContent": "ciphertext",
                        }
                    })),
                },
                aimux_core::result::GenerateContent::Text {
                    text: "working".to_owned(),
                    provider_metadata: None,
                },
                aimux_core::result::GenerateContent::ToolCall {
                    tool_call_id: "call_1".to_owned(),
                    tool_name: "bash".to_owned(),
                    input: serde_json::json!({"cmd": "pwd"}),
                    provider_executed: None,
                    dynamic: None,
                    thought_signature: None,
                    provider_metadata: None,
                },
            ],
            finish_reason: aimux_core::types::FinishReason {
                unified: FinishReasonUnified::ToolCalls,
                raw: Some("tool_calls".to_owned()),
            },
            usage: aimux_core::types::Usage {
                input_tokens: aimux_core::types::TokenUsage {
                    total: Some(41),
                    ..Default::default()
                },
                output_tokens: aimux_core::types::TokenUsage {
                    total: Some(12),
                    text: Some(3),
                    reasoning: Some(9),
                    ..Default::default()
                },
                raw: None,
            },
            warnings: Vec::new(),
            provider_metadata: None,
            response: Default::default(),
            request_body: None,
            response_headers: None,
        };

        let output_items = std::sync::Arc::new(vec![
            serde_json::json!({
                "id": "rs_1",
                "type": "reasoning",
                "status": "completed",
                "encrypted_content": "ciphertext",
                "summary": [],
            }),
            serde_json::json!({
                "id": "msg_1",
                "type": "message",
                "status": "completed",
                "role": "assistant",
                "content": [{ "type": "output_text", "text": "working", "annotations": [] }],
            }),
            serde_json::json!({
                "id": "fc_1",
                "type": "function_call",
                "status": "completed",
                "call_id": "call_1",
                "name": "bash",
                "arguments": "{\"cmd\":\"pwd\"}",
            }),
        ]);
        let outcome = model_outcome_from_generate_result(
            result,
            &execution("openai-responses", false),
            Some(output_items.clone()),
            full_input_diagnostics(1, 1),
        )
        .expect("valid non-streaming result");

        assert_eq!(outcome.text, "working");
        assert_eq!(outcome.tool_calls.len(), 1);
        assert_eq!(outcome.tool_calls[0].tool_call_id, "call_1");
        assert_eq!(outcome.tool_calls[0].tool_name, "bash");
        assert_eq!(
            outcome.tool_calls[0].arguments,
            serde_json::json!({"cmd": "pwd"})
        );
        let context = outcome.provider_context.expect("provider context");
        assert_eq!(context.output_items.as_ref(), output_items.as_ref());
        assert_eq!(context.output_items.len(), 3);
        assert_eq!(context.output_items[0]["id"], "rs_1");
        assert_eq!(context.output_items[0]["encrypted_content"], "ciphertext");
        let usage = outcome.usage.expect("exact provider usage");
        assert_eq!(usage.input_tokens, 41);
        assert_eq!(usage.cached_input_tokens, None);
        assert_eq!(usage.output_tokens, 12);
        assert_eq!(usage.output_reasoning_tokens, Some(9));
        assert_eq!(usage.output_text_tokens, Some(3));
    }

    #[test]
    // Contract: docs/zork-agent-architecture.md [PROVIDER-01]
    fn non_streaming_responses_accepts_plain_raw_reasoning() {
        let result = aimux_core::result::GenerateResult {
            content: vec![aimux_core::result::GenerateContent::Reasoning {
                text: "summary".to_owned(),
                provider_metadata: Some(serde_json::json!({
                    "openai": { "itemId": "rs_1" }
                })),
            }],
            finish_reason: aimux_core::types::FinishReason {
                unified: FinishReasonUnified::Stop,
                raw: Some("stop".to_owned()),
            },
            usage: Default::default(),
            warnings: Vec::new(),
            provider_metadata: None,
            response: Default::default(),
            request_body: None,
            response_headers: None,
        };

        let plain = std::sync::Arc::new(vec![serde_json::json!({
            "id": "rs_1",
            "type": "reasoning",
            "status": null,
            "summary": [],
            "content": [{ "type": "reasoning_text", "text": "summary" }],
            "encrypted_content": null,
        })]);
        let outcome = model_outcome_from_generate_result(
            result,
            &execution("openai-responses", false),
            Some(plain.clone()),
            full_input_diagnostics(1, 1),
        )
        .expect("plain reasoning is replayable as a raw output item");

        assert_eq!(
            outcome.provider_context.unwrap().output_items.as_ref(),
            plain.as_ref()
        );
    }

    #[test]
    // Contract: docs/zork-agent-architecture.md [PROVIDER-01, PROVIDER-03]
    fn responses_options_preserve_profile_defined_reasoning_exactly() {
        let options = responses_provider_options("future-depth");

        assert_eq!(options["openai"]["reasoningEffort"], "future-depth");
        assert_eq!(options["openai"]["forceReasoning"], true);
        assert_eq!(options["openai"]["store"], false);

        let call_options = CallOptions {
            provider_options: Some(options),
            ..CallOptions::new(vec![LanguageModelPromptMessage {
                role: Role::User,
                content: vec![ContentPart::text("test")],
                provider_options: None,
            }])
        };
        let request = aimux_providers::openai::responses::build_responses_request_body(
            "gpt-5.6-luna",
            &call_options,
            true,
        );
        assert_eq!(request.body["reasoning"]["effort"], "future-depth");
        assert_eq!(request.body["store"], false);
    }
}
