//! Pure projection from the current generation to provider messages.

use std::sync::Arc;

use serde_json::json;

use super::events::{ToolDelivery, ToolDeliveryMode, ToolOutcome};
use super::state::{GenerationEntry, SessionState};
use super::wire::{ProviderMessage, TranscriptRole};

pub fn provider_transcript(state: &SessionState) -> Arc<Vec<ProviderMessage>> {
    let mut messages = Vec::new();
    if let Some(prompt) = state
        .system_prompt
        .as_deref()
        .filter(|prompt| !prompt.is_empty())
    {
        messages.push(message(TranscriptRole::System, prompt, false));
    }
    messages.push(message(
        TranscriptRole::System,
        &tool_catalog_prompt(state),
        false,
    ));
    if let Some(document) = state.generation.document.as_deref() {
        messages.push(message(
            TranscriptRole::System,
            &format!("Context from the previous generation:\n\n{document}"),
            false,
        ));
    }

    for entry in &state.generation.entries {
        match entry {
            GenerationEntry::Inputs { inputs } => {
                for input in inputs {
                    messages.push(message(TranscriptRole::User, &input.content, false));
                }
            }
            GenerationEntry::ToolChanges { changes } => {
                let lines = changes
                    .iter()
                    .map(|change| match change {
                        super::tools::ToolChange::Added { name, .. } => format!(
                            "Tool {name} was added. If you need it, call tool.help for its current usage."
                        ),
                        super::tools::ToolChange::Updated { name, .. } => format!(
                            "Tool {name} was updated. If you need it, call tool.help for its current usage."
                        ),
                        super::tools::ToolChange::Removed { name } => {
                            format!("Tool {name} was removed.")
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                messages.push(runtime_notice(&lines));
            }
            GenerationEntry::Notice { message } => messages.push(runtime_notice(message)),
            GenerationEntry::Outstanding { items } => {
                messages.push(runtime_notice(&format!(
                    "Unfinished items:\n{}",
                    serde_json::to_string_pretty(items).unwrap_or_else(|_| "[]".into())
                )));
            }
            GenerationEntry::ToolDelivery { delivery } => match delivery {
                ToolDelivery::Pending { invocation } => {
                    messages.push(ProviderMessage {
                        role: TranscriptRole::Tool,
                        content: Arc::from(
                            "This tool invocation was still unfinished when the runtime continued. Its real completion or failure will be delivered later as a new runtime notification.",
                        ),
                        is_error: true,
                        tool_call_id: Some(invocation.provider_call_id.clone()),
                        tool_calls: Vec::new(),
                        provider_context: None,
                    });
                }
                ToolDelivery::Result {
                    invocation,
                    result,
                    mode: ToolDeliveryMode::Direct,
                } => {
                    messages.push(ProviderMessage {
                        role: TranscriptRole::Tool,
                        content: Arc::from(render_result(result)),
                        is_error: result.outcome != ToolOutcome::Succeeded,
                        tool_call_id: Some(invocation.provider_call_id.clone()),
                        tool_calls: Vec::new(),
                        provider_context: None,
                    });
                }
                ToolDelivery::Result {
                    invocation,
                    result,
                    mode: ToolDeliveryMode::Notification,
                } => messages.push(runtime_notice(&format!(
                    "A previously unfinished tool invocation has now returned. invocation_id={} tool={} result={}",
                    invocation.invocation_id,
                    invocation.tool,
                    render_result(result)
                ))),
            },
            GenerationEntry::Assistant {
                text,
                provider_calls,
                provider_context,
                ..
            } => messages.push(ProviderMessage {
                role: TranscriptRole::Assistant,
                content: Arc::from(text.as_str()),
                is_error: false,
                tool_call_id: None,
                tool_calls: provider_calls.clone(),
                provider_context: provider_context.clone(),
            }),
            GenerationEntry::CarriedTools { invocations } => {
                let carried = invocations
                    .iter()
                    .map(|invocation| {
                        json!({
                            "invocation_id": invocation.invocation_id,
                            "turn_id": invocation.turn_id,
                            "tool": invocation.tool,
                            "arguments": invocation.arguments,
                            "started_at_ms": invocation.started_at_ms,
                        })
                    })
                    .collect::<Vec<_>>();
                messages.push(runtime_notice(&format!(
                    "These tool invocations were unfinished at the context transition. They are not recreated as provider call/result pairs. Later outcomes will arrive as notifications:\n{}",
                    serde_json::to_string_pretty(&carried).unwrap_or_else(|_| "[]".into())
                )));
            }
        }
    }
    Arc::new(messages)
}

fn tool_catalog_prompt(state: &SessionState) -> String {
    let mut text = String::from(
        "Logical tools are called through the single provider tool `call` with {\"tool\":\"complete.name\",\"arguments\":{...}}. Use tool.help when you need a tool's current detailed usage.\n\nTools known at the start of this generation:\n",
    );
    for tool in &state.generation.tools {
        text.push_str(&format!(
            "- {} (version {}): {}\n",
            tool.name, tool.version, tool.description
        ));
    }
    text
}

fn render_result(result: &super::events::ToolResultData) -> String {
    serde_json::to_string(&json!({
        "invocation_id": result.invocation_id,
        "tool": result.tool,
        "outcome": result.outcome,
        "data": result.data,
    }))
    .unwrap_or_else(|_| format!("tool {} returned {:?}", result.tool, result.outcome))
}

fn runtime_notice(content: &str) -> ProviderMessage {
    message(
        TranscriptRole::User,
        &format!("[zork-agent runtime notification]\n{content}"),
        false,
    )
}

fn message(role: TranscriptRole, content: &str, is_error: bool) -> ProviderMessage {
    ProviderMessage {
        role,
        content: Arc::from(content),
        is_error,
        tool_call_id: None,
        tool_calls: Vec::new(),
        provider_context: None,
    }
}
