use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use aimux_core::{
    error::AiMuxError,
    options::CallOptions,
    result::{GenerateResult, StreamResult},
    stream_part::StreamPart,
};
use aimux_provider_utils::{
    send_stream_timed, HttpBody, HttpMethod, HttpRequest, RetryConfig, DEFAULT_ERROR_STRUCTURE,
};
use aimux_providers::openai::responses::{
    build_responses_request_body,
    responses_convert::{
        build_header_list, build_responses_event_stream, build_responses_generate_result,
    },
};
use aimux_stream::{SseError, SseEvent, SseStream};
use futures_util::{StreamExt, TryStreamExt};
use serde_json::Value;

use zork_agent::session::ports::ProfileExecution;

pub(super) struct ResponsesGenerateResult {
    pub(super) result: GenerateResult,
    pub(super) output_items: Arc<Vec<Value>>,
}

pub(super) struct ResponsesStreamResult {
    pub(super) result: StreamResult,
    pub(super) output_items: CapturedOutputItems,
}

#[derive(Clone, Default)]
pub(super) struct CapturedOutputItems {
    state: Arc<Mutex<OutputItemsState>>,
}

#[derive(Default)]
struct OutputItemsState {
    slots: Vec<Option<Value>>,
    error: Option<String>,
    terminal_received: bool,
}

impl CapturedOutputItems {
    fn observe(&self, event: &Result<SseEvent, SseError>) {
        let Ok(event) = event else {
            return;
        };
        let Ok(document) = serde_json::from_str::<Value>(&event.data) else {
            return;
        };
        match document.get("type").and_then(Value::as_str) {
            Some("response.output_item.done") => {
                let mut state = self
                    .state
                    .lock()
                    .expect("output item capture mutex poisoned");
                let Some(index) = document
                    .get("output_index")
                    .and_then(Value::as_u64)
                    .and_then(|index| usize::try_from(index).ok())
                else {
                    state.error.get_or_insert_with(|| {
                        "response.output_item.done has no valid output_index".to_owned()
                    });
                    return;
                };
                let Some(item) = document
                    .get("item")
                    .filter(|item| item.is_object())
                    .cloned()
                else {
                    state.error.get_or_insert_with(|| {
                        "response.output_item.done has no object item".to_owned()
                    });
                    return;
                };
                if state.slots.len() <= index {
                    state.slots.resize(index + 1, None);
                }
                if state.slots[index].replace(item).is_some() {
                    state.error.get_or_insert_with(|| {
                        "provider completed the same output index more than once".to_owned()
                    });
                }
            }
            Some("response.completed" | "response.incomplete") => {
                let mut state = self
                    .state
                    .lock()
                    .expect("output item capture mutex poisoned");
                state.terminal_received = true;
                if state.slots.is_empty() {
                    let items = document
                        .pointer("/response/output")
                        .and_then(Value::as_array);
                    if let Some(items) = items {
                        state.slots = items.iter().cloned().map(Some).collect();
                    }
                }
            }
            Some("response.failed" | "error") => {
                self.state
                    .lock()
                    .expect("output item capture mutex poisoned")
                    .terminal_received = true;
            }
            _ => {}
        }
    }

    pub(super) fn terminal_received(&self) -> bool {
        self.state
            .lock()
            .expect("output item capture mutex poisoned")
            .terminal_received
    }

    pub(super) fn ordered(&self) -> Result<Arc<Vec<Value>>, String> {
        let state = self
            .state
            .lock()
            .expect("output item capture mutex poisoned");
        if let Some(error) = &state.error {
            return Err(error.clone());
        }
        state
            .slots
            .iter()
            .enumerate()
            .map(|(index, item)| {
                item.clone()
                    .ok_or_else(|| format!("provider output has a gap at index {index}"))
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Arc::new)
    }
}

pub(super) async fn do_generate(
    options: &CallOptions,
    execution: &ProfileExecution,
    input: Vec<Value>,
) -> Result<ResponsesGenerateResult, AiMuxError> {
    let request = build_responses_request_body(execution.model(), options, false);
    let warnings = request.warnings;
    let mut body = request.body;
    let object = body
        .as_object_mut()
        .expect("aimux Responses request body must be an object");
    object.insert("input".to_owned(), Value::Array(input));
    object.insert(
        "parallel_tool_calls".to_owned(),
        Value::Bool(execution.parallel_tool_calls()),
    );
    let provider_key = if execution.provider().contains("azure") {
        "azure"
    } else {
        "openai"
    };
    let mut headers = HashMap::from([(
        "Authorization".to_owned(),
        format!("Bearer {}", execution.secret()),
    )]);
    headers.extend(execution.headers().clone());
    let response = send_stream_timed(
        HttpRequest {
            method: HttpMethod::Post,
            url: format!("{}/responses", execution.base_url().trim_end_matches('/')),
            headers: build_header_list(&headers),
            body: HttpBody::Json(body),
            abort_signal: options.abort_signal.clone(),
            call_id: options.call_id.clone(),
            recording_context: options.recording_context.clone(),
        },
        RetryConfig {
            max_retries: 0,
            ..RetryConfig::default()
        },
        &DEFAULT_ERROR_STRUCTURE,
        options.timeout.map(Into::into),
    )
    .await?;

    let status = response.status;
    let response_headers = response.headers;
    let response_body = response
        .body
        .map_ok(|chunk| chunk.to_vec())
        .try_concat()
        .await?;
    let data: Value = serde_json::from_slice(&response_body)?;
    let output_items = Arc::new(
        data.get("output")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
    );
    let mut result = build_responses_generate_result(
        &data,
        status,
        &String::from_utf8_lossy(&response_body),
        warnings,
        provider_key.to_owned(),
        Value::Null,
        response_headers,
    )?;
    result.request_body = None;
    Ok(ResponsesGenerateResult {
        result,
        output_items,
    })
}

pub(super) async fn do_stream(
    options: &CallOptions,
    execution: &ProfileExecution,
    input: Vec<Value>,
) -> Result<ResponsesStreamResult, AiMuxError> {
    let request = build_responses_request_body(execution.model(), options, true);
    let warnings = request.warnings;
    let mut body = request.body;
    let object = body
        .as_object_mut()
        .expect("aimux Responses request body must be an object");
    object.insert("input".to_owned(), Value::Array(input));
    object.insert(
        "parallel_tool_calls".to_owned(),
        Value::Bool(execution.parallel_tool_calls()),
    );
    let provider_key = if execution.provider().contains("azure") {
        "azure"
    } else {
        "openai"
    };
    let mut headers = HashMap::from([(
        "Authorization".to_owned(),
        format!("Bearer {}", execution.secret()),
    )]);
    headers.extend(execution.headers().clone());
    let response = send_stream_timed(
        HttpRequest {
            method: HttpMethod::Post,
            url: format!("{}/responses", execution.base_url().trim_end_matches('/')),
            headers: build_header_list(&headers),
            body: HttpBody::Json(body),
            abort_signal: options.abort_signal.clone(),
            call_id: options.call_id.clone(),
            recording_context: options.recording_context.clone(),
        },
        RetryConfig {
            max_retries: 0,
            ..RetryConfig::default()
        },
        &DEFAULT_ERROR_STRUCTURE,
        options.timeout.map(Into::into),
    )
    .await?;

    let status = response.status;
    let response_headers = response.headers;
    let final_reasoning = Arc::new(Mutex::new(HashMap::<String, String>::new()));
    let output_items = CapturedOutputItems::default();
    let mut sse = SseStream::with_max_event_size(response.body, usize::MAX);
    let first_event = sse.next().await;
    if let Some(event) = &first_event {
        observe_final_reasoning(event, &final_reasoning);
        output_items.observe(event);
    }
    let observed = sse.map({
        let final_reasoning = final_reasoning.clone();
        let output_items = output_items.clone();
        move |event| {
            observe_final_reasoning(&event, &final_reasoning);
            output_items.observe(&event);
            event
        }
    });
    let stream = build_responses_event_stream(
        first_event,
        observed,
        status,
        provider_key.to_owned(),
        warnings,
        false,
    )?;
    let stream = stream.map({
        let final_reasoning = final_reasoning.clone();
        move |part| part.map(|part| merge_final_reasoning(part, &final_reasoning))
    });

    Ok(ResponsesStreamResult {
        result: StreamResult {
            stream: Box::pin(stream),
            request_body: None,
            response_headers: Some(response_headers),
        },
        output_items,
    })
}

fn observe_final_reasoning(
    event: &Result<SseEvent, SseError>,
    final_reasoning: &Mutex<HashMap<String, String>>,
) {
    let Ok(event) = event else {
        return;
    };
    if !event.data.contains("response.output_item.done")
        || !event.data.contains("encrypted_content")
    {
        return;
    }
    let Ok(document) = serde_json::from_str::<Value>(&event.data) else {
        return;
    };
    if document.get("type").and_then(Value::as_str) != Some("response.output_item.done") {
        return;
    }
    let Some(item) = document.get("item") else {
        return;
    };
    if item.get("type").and_then(Value::as_str) != Some("reasoning") {
        return;
    }
    let Some(item_id) = item.get("id").and_then(Value::as_str) else {
        return;
    };
    let Some(encrypted_content) = item.get("encrypted_content").and_then(Value::as_str) else {
        return;
    };
    if item_id.is_empty() || encrypted_content.is_empty() {
        return;
    }
    final_reasoning
        .lock()
        .expect("final reasoning mutex poisoned")
        .insert(item_id.to_owned(), encrypted_content.to_owned());
}

fn merge_final_reasoning(
    part: StreamPart,
    final_reasoning: &Mutex<HashMap<String, String>>,
) -> StreamPart {
    let StreamPart::ReasoningEnd {
        id,
        mut provider_metadata,
    } = part
    else {
        return part;
    };
    let mut final_reasoning = final_reasoning
        .lock()
        .expect("final reasoning mutex poisoned");
    if let Some(metadata) = provider_metadata.as_mut() {
        for options in metadata
            .as_object_mut()
            .into_iter()
            .flat_map(|providers| providers.values_mut())
        {
            let Some(item_id) = options.get("itemId").and_then(Value::as_str) else {
                continue;
            };
            if let Some(encrypted_content) = final_reasoning.remove(item_id) {
                options["reasoningEncryptedContent"] = Value::String(encrypted_content);
            }
        }
    }
    StreamPart::ReasoningEnd {
        id,
        provider_metadata,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output_item_done(index: usize, item: Value) -> Result<SseEvent, SseError> {
        Ok(SseEvent {
            data: serde_json::json!({
                "type": "response.output_item.done",
                "output_index": index,
                "item": item,
            })
            .to_string(),
            ..Default::default()
        })
    }

    #[test]
    // Contract: docs/zork-agent-architecture.md [PROVIDER-01, PROJECTION-02]
    fn raw_output_items_follow_provider_output_index_not_arrival_order() {
        let captured = CapturedOutputItems::default();
        captured.observe(&output_item_done(
            1,
            serde_json::json!({ "id": "fc_1", "type": "function_call" }),
        ));
        captured.observe(&output_item_done(
            0,
            serde_json::json!({ "id": "rs_1", "type": "reasoning" }),
        ));

        assert_eq!(
            captured.ordered().unwrap().as_ref(),
            &vec![
                serde_json::json!({ "id": "rs_1", "type": "reasoning" }),
                serde_json::json!({ "id": "fc_1", "type": "function_call" }),
            ]
        );
    }

    #[test]
    // Contract: docs/zork-agent-architecture.md [PROVIDER-01]
    fn raw_output_items_reject_a_missing_output_index() {
        let captured = CapturedOutputItems::default();
        captured.observe(&output_item_done(
            1,
            serde_json::json!({ "id": "fc_1", "type": "function_call" }),
        ));

        assert_eq!(
            captured.ordered().unwrap_err(),
            "provider output has a gap at index 0"
        );
    }

    #[test]
    // Contract: docs/zork-agent-architecture.md [PROVIDER-01]
    fn terminal_state_requires_a_terminal_responses_event() {
        let captured = CapturedOutputItems::default();
        captured.observe(&output_item_done(
            0,
            serde_json::json!({ "id": "msg_1", "type": "message" }),
        ));
        assert!(!captured.terminal_received());

        captured.observe(&Ok(SseEvent {
            data: serde_json::json!({
                "type": "response.completed",
                "response": { "output": [] },
            })
            .to_string(),
            ..Default::default()
        }));
        assert!(captured.terminal_received());
    }

    #[test]
    // Contract: docs/zork-agent-architecture.md [PROVIDER-01]
    fn merging_reasoning_releases_the_stream_side_ciphertext() {
        let final_reasoning = Mutex::new(HashMap::from([(
            "rs_1".to_owned(),
            "ciphertext".to_owned(),
        )]));
        let merged = merge_final_reasoning(
            StreamPart::ReasoningEnd {
                id: "rs_1:0".to_owned(),
                provider_metadata: Some(serde_json::json!({
                    "openai": { "itemId": "rs_1" }
                })),
            },
            &final_reasoning,
        );

        let StreamPart::ReasoningEnd {
            provider_metadata: Some(metadata),
            ..
        } = merged
        else {
            panic!("expected reasoning end");
        };
        assert_eq!(
            metadata["openai"]["reasoningEncryptedContent"],
            "ciphertext"
        );
        assert!(
            final_reasoning
                .lock()
                .expect("final reasoning mutex poisoned")
                .is_empty(),
            "ciphertext is no longer needed after it is moved into the emitted part",
        );
    }
}
