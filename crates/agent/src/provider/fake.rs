use std::borrow::Cow;

use serde_json::Value;
use zork_agent::session::events::END_TOOL_NAME;
use zork_agent::session::model::{ModelError, ModelOutcome, ModelRequest};
use zork_agent::session::ports::{ModelExecutor, ProfileExecution};
use zork_agent::session::tools::PROVIDER_CALL_NAME;
use zork_agent::session::wire::{ProviderToolCall, TranscriptRole};

pub struct FakeProvider;

impl ModelExecutor for FakeProvider {
    fn complete<'a>(
        &'a self,
        request: &'a ModelRequest,
        _execution: ProfileExecution,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<ModelOutcome, ModelError>> + Send + 'a>,
    > {
        Box::pin(async move {
            let latest = request.transcript.last();
            if latest.is_some_and(|message| message.role == TranscriptRole::Assistant)
                && request
                    .tools
                    .iter()
                    .any(|definition| definition.name == PROVIDER_CALL_NAME)
            {
                return Ok(ModelOutcome {
                    text: String::new(),
                    tool_calls: vec![dynamic_call(
                        format!("fake-end:{}", request.step_id),
                        END_TOOL_NAME,
                        serde_json::json!({}),
                    )],
                    provider_context: None,
                    usage: None,
                    provider_input: None,
                });
            }
            if let Some(message) = latest.filter(|message| {
                matches!(message.role, TranscriptRole::User | TranscriptRole::Tool)
            }) {
                let input_content = fake_input_content(&message.content);
                if let Some(Value::Object(document)) = fake_input_document(&input_content) {
                    if let Some(Value::Array(tools)) = document.get("fake_tools") {
                        let calls = tools
                            .iter()
                            .enumerate()
                            .filter_map(|(index, tool)| {
                                let Value::Object(tool) = tool else {
                                    return None;
                                };
                                let name = tool.get("name")?.as_str()?;
                                let input = tool.get("input")?.clone();
                                input.is_object().then_some((index, name, input))
                            })
                            .map(|(index, name, input)| {
                                dynamic_call(
                                    format!("fake-tool:{}:{index}", request.step_id),
                                    name,
                                    input,
                                )
                            })
                            .collect::<Vec<_>>();
                        if !calls.is_empty() {
                            return Ok(ModelOutcome {
                                text: String::new(),
                                tool_calls: calls,
                                provider_context: None,
                                usage: None,
                                provider_input: None,
                            });
                        }
                    }
                    if let Some(Value::Object(tool)) = document.get("fake_tool") {
                        let name = tool.get("name").and_then(Value::as_str);
                        let input = tool.get("input").cloned();
                        if let (Some(name), Some(input)) = (name, input) {
                            if input.is_object() {
                                return Ok(ModelOutcome {
                                    text: String::new(),
                                    tool_calls: vec![dynamic_call(
                                        format!("fake-tool:{}", request.step_id),
                                        name,
                                        input,
                                    )],
                                    provider_context: None,
                                    usage: None,
                                    provider_input: None,
                                });
                            }
                        }
                    }
                }
            }
            let text = latest
                .filter(|message| {
                    matches!(message.role, TranscriptRole::User | TranscriptRole::Tool)
                })
                .map(|message| fake_input_content(&message.content).into_owned())
                .unwrap_or_default();
            request.stream_observer.text_delta(
                &request.session_id,
                request.generation,
                &request.step_id,
                &text,
            );
            Ok(ModelOutcome {
                text,
                tool_calls: Vec::new(),
                provider_context: None,
                usage: None,
                provider_input: None,
            })
        })
    }
}

fn dynamic_call(tool_call_id: String, tool: &str, arguments: Value) -> ProviderToolCall {
    ProviderToolCall {
        tool_call_id,
        tool_name: PROVIDER_CALL_NAME.to_owned(),
        arguments: serde_json::json!({
            "tool": tool,
            "arguments": arguments,
        }),
    }
}

fn fake_input_content(content: &str) -> Cow<'_, str> {
    serde_json::from_str::<Value>(content)
        .ok()
        .and_then(|value| {
            value
                .get("messages")
                .and_then(Value::as_array)
                .and_then(|messages| messages.last())
                .and_then(|message| message.get("content"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .map(Cow::Owned)
        .unwrap_or(Cow::Borrowed(content))
}

fn fake_input_document(content: &str) -> Option<Value> {
    serde_json::from_str(content).ok()
}
