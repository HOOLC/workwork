use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use reqwest::StatusCode;
use serde_json::{json, Value};
use zork_agent::provider::ProviderRouter;
use zork_agent::session::events::{SessionEvent, TurnOutcome};
use zork_agent::session::model::{
    ModelError, ModelRequest, ModelStreamObserver, SilentStreamObserver,
};
use zork_agent::session::ports::{ModelExecutor, ModelLimits, ProfileExecution};
use zork_agent::session::wire::{ProviderMessage, SessionSelection, TranscriptRole};
use zork_agent_testkit::{
    ControlledHttpProvider, PausedTimeIoGuard, PendingHttpRequest, RealAgent,
};

const MODEL: &str = "responses-model";

fn selection(profile_id: &str) -> SessionSelection {
    SessionSelection {
        profile_id: profile_id.into(),
        model: MODEL.into(),
        thinking: "xhigh".into(),
    }
}

fn profile(provider_base_url: &str, streaming: bool) -> Value {
    json!({
        "provider": "openai-compatible",
        "billing": "usage",
        "base_url": format!("{provider_base_url}/v1"),
        "headers": {"x-profile-header": "responses-fixture"},
        "auth": {"type": "api_key", "key": "responses-secret"},
        "models": [{
            "id": MODEL,
            "api": "openai-responses",
            "streaming": streaming,
            "parallel_tool_calls": false,
            "thinking": ["xhigh"],
            "default_thinking": "xhigh",
            "capabilities": {"input": ["text"]},
            "limits": {"context_window_tokens": 1000000, "max_output_tokens": 56000},
            "default": true
        }]
    })
}

fn completed(response_id: &str, input_tokens: u64, output_tokens: u64) -> Value {
    json!({
        "type": "response.completed",
        "response": {
            "id": response_id,
            "created_at": 1,
            "model": MODEL,
            "incomplete_details": null,
            "usage": {
                "input_tokens": input_tokens,
                "input_tokens_details": {"cached_tokens": input_tokens.saturating_sub(1)},
                "output_tokens": output_tokens,
                "output_tokens_details": {"reasoning_tokens": output_tokens.saturating_sub(1)}
            }
        }
    })
}

fn text_events(response_id: &str, text: &str) -> Vec<Value> {
    let item_id = format!("msg_{response_id}");
    vec![
        json!({
            "type": "response.created",
            "response": {"id": response_id, "created_at": 1, "model": MODEL}
        }),
        json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {
                "id": item_id,
                "type": "message",
                "status": "in_progress",
                "role": "assistant",
                "content": []
            }
        }),
        json!({
            "type": "response.output_text.delta",
            "item_id": item_id,
            "output_index": 0,
            "delta": text
        }),
        json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": {
                "id": item_id,
                "type": "message",
                "status": "completed",
                "role": "assistant",
                "content": [{"type": "output_text", "text": text, "annotations": []}]
            }
        }),
        completed(response_id, 123, 45),
    ]
}

fn respond_text(request: PendingHttpRequest, response_id: &str, text: &str) {
    request.respond_sse(text_events(response_id, text)).unwrap();
}

fn response_call_item(item_id: &str, call_id: &str, tool: &str, arguments: Value) -> Value {
    json!({
        "id": item_id,
        "type": "function_call",
        "status": "completed",
        "arguments": json!({"tool": tool, "arguments": arguments}).to_string(),
        "call_id": call_id,
        "name": "call"
    })
}

fn respond_call(
    request: PendingHttpRequest,
    response_id: &str,
    item_id: &str,
    call_id: &str,
    tool: &str,
    arguments: Value,
) {
    let item = response_call_item(item_id, call_id, tool, arguments);
    let arguments = item["arguments"].clone();
    request
        .respond_sse([
            json!({
                "type": "response.created",
                "response": {"id": response_id, "created_at": 1, "model": MODEL}
            }),
            json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {
                    "id": item_id,
                    "type": "function_call",
                    "status": "in_progress",
                    "arguments": "",
                    "call_id": call_id,
                    "name": "call"
                }
            }),
            json!({
                "type": "response.function_call_arguments.delta",
                "item_id": item_id,
                "output_index": 0,
                "delta": arguments
            }),
            json!({
                "type": "response.function_call_arguments.done",
                "item_id": item_id,
                "output_index": 0,
                "arguments": arguments
            }),
            json!({"type": "response.output_item.done", "output_index": 0, "item": item}),
            completed(response_id, 10, 2),
        ])
        .unwrap();
}

fn reasoning_item(id: &str, encrypted: bool) -> Value {
    if encrypted {
        json!({
            "id": id,
            "type": "reasoning",
            "status": "completed",
            "encrypted_content": "encrypted-reasoning",
            "summary": [{"type": "summary_text", "text": "Inspect the file."}]
        })
    } else {
        json!({
            "id": id,
            "type": "reasoning",
            "status": null,
            "summary": [],
            "content": [{"type": "reasoning_text", "text": "Inspect the file before answering.\n"}],
            "encrypted_content": null
        })
    }
}

fn respond_reasoning_and_read(
    request: PendingHttpRequest,
    response_id: &str,
    reasoning_id: &str,
    call_item_id: &str,
    call_id: &str,
    encrypted: bool,
) -> Vec<Value> {
    let reasoning = reasoning_item(reasoning_id, encrypted);
    let call = response_call_item(
        call_item_id,
        call_id,
        "file.read",
        json!({"path": "README.md"}),
    );
    let call_arguments = call["arguments"].clone();
    request
        .respond_sse([
            json!({
                "type": "response.created",
                "response": {"id": response_id, "created_at": 1, "model": MODEL}
            }),
            json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {
                    "id": reasoning_id,
                    "type": "reasoning",
                    "status": "in_progress",
                    "summary": []
                }
            }),
            json!({"type": "response.output_item.done", "output_index": 0, "item": reasoning}),
            json!({
                "type": "response.output_item.added",
                "output_index": 1,
                "item": {
                    "id": call_item_id,
                    "type": "function_call",
                    "status": "in_progress",
                    "arguments": "",
                    "call_id": call_id,
                    "name": "call"
                }
            }),
            json!({
                "type": "response.function_call_arguments.delta",
                "item_id": call_item_id,
                "output_index": 1,
                "delta": call_arguments
            }),
            json!({
                "type": "response.function_call_arguments.done",
                "item_id": call_item_id,
                "output_index": 1,
                "arguments": call_arguments
            }),
            json!({"type": "response.output_item.done", "output_index": 1, "item": call}),
            completed(response_id, 200, 60),
        ])
        .unwrap();
    vec![reasoning, call]
}

fn input(body: &Value) -> &[Value] {
    body["input"].as_array().unwrap()
}

fn assert_raw_items(body: &Value, raw_items: &[Value], call_id: &str) {
    let input = input(body);
    let start = input
        .iter()
        .position(|item| item.get("id") == raw_items[0].get("id"))
        .expect("raw provider context is present");
    assert_eq!(&input[start..start + raw_items.len()], raw_items);
    assert!(input[start + raw_items.len()..]
        .iter()
        .any(|item| { item["type"] == "function_call_output" && item["call_id"] == call_id }));
}

fn non_streaming_response(output: Vec<Value>, input_tokens: u64, output_tokens: u64) -> Value {
    json!({
        "id": "resp_non_streaming",
        "object": "response",
        "created_at": 1,
        "model": MODEL,
        "status": "completed",
        "incomplete_details": null,
        "output": output,
        "usage": {
            "input_tokens": input_tokens,
            "input_tokens_details": {"cached_tokens": input_tokens.saturating_sub(1)},
            "output_tokens": output_tokens,
            "output_tokens_details": {"reasoning_tokens": output_tokens.saturating_sub(1)}
        }
    })
}

async fn advance_real_transport(duration: Duration) {
    let mut remaining = duration;
    while !remaining.is_zero() {
        let step = remaining.min(Duration::from_secs(1));
        tokio::time::advance(step).await;
        tokio::task::yield_now().await;
        remaining -= step;
    }
}

#[derive(Debug)]
struct DeltaObserver {
    deltas: tokio::sync::mpsc::UnboundedSender<String>,
}

impl ModelStreamObserver for DeltaObserver {
    fn text_delta(&self, _: &str, _: u64, _: &str, text: &str) {
        let _ = self.deltas.send(text.to_owned());
    }
}

fn provider_execution(base_url: &str, profile_id: &str, streaming: bool) -> ProfileExecution {
    ProfileExecution::new(
        profile_id.to_owned(),
        "openai-compatible".to_owned(),
        MODEL.to_owned(),
        "openai-responses".to_owned(),
        streaming,
        false,
        None,
        format!("{base_url}/v1"),
        HashMap::new(),
        "xhigh".to_owned(),
        ModelLimits {
            context_window_tokens: 1_000_000,
            max_output_tokens: 56_000,
            reserve_percent: 10,
        },
        "responses-secret".to_owned(),
    )
}

fn provider_request(profile_id: &str, observer: Arc<dyn ModelStreamObserver>) -> ModelRequest {
    ModelRequest {
        session_id: "session".to_owned(),
        generation: 1,
        step_id: "step".to_owned(),
        selection: selection(profile_id),
        transcript: Arc::new(vec![ProviderMessage {
            role: TranscriptRole::User,
            content: "wait for the provider".into(),
            is_error: false,
            tool_call_id: None,
            tool_calls: Vec::new(),
            provider_context: None,
        }]),
        tools: Arc::new(Vec::new()),
        max_output_tokens: Some(56000),
        independent: false,
        stream_observer: observer,
    }
}

#[tokio::test(flavor = "multi_thread")]
// Contract: docs/zork-agent-architecture.md [COMPACTION-01, PROVIDER-01, PROVIDER-02]
async fn compaction_is_tool_free_on_responses_and_chat_completions() {
    for api in ["openai-responses", "openai-completions"] {
        let mut agent = RealAgent::new().unwrap();
        let mut settings = profile(agent.provider_base_url(), true);
        settings["models"][0]["api"] = json!(api);
        settings["models"][0]["limits"] =
            json!({"context_window_tokens": 256_000, "max_output_tokens": 131_072});
        agent.install_profile("compact", settings).unwrap();
        let session = agent
            .create_configured_session(selection("compact"), None)
            .await
            .unwrap();
        agent
            .send_mail(&session, "Keep working on the same task")
            .await
            .unwrap();
        let normal = agent.request().await;
        if api == "openai-responses" {
            assert_eq!(normal.json().unwrap()["max_output_tokens"], 131_072);
            let mut events = text_events("normal", "progress so far");
            *events.last_mut().unwrap() = completed("normal", 123_000, 20);
            normal.respond_sse(events).unwrap();
        } else {
            normal.respond_sse([json!({"id": "normal", "object": "chat.completion.chunk",
                "choices": [{"index": 0, "delta": {"role": "assistant", "content": "progress so far"}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 123_000, "completion_tokens": 20, "total_tokens": 123_020}})]).unwrap();
        }
        let summary = tokio::time::timeout(Duration::from_secs(3), agent.request())
            .await
            .unwrap();
        let body = summary.json().unwrap();
        if api == "openai-responses" {
            assert_eq!(body["max_output_tokens"], 131_072);
        }
        assert!(
            body.get("tools")
                .is_none_or(|value| value.is_null() || value.as_array().is_some_and(Vec::is_empty)),
            "{api}: {body}"
        );
        assert!(body.get("previous_response_id").is_none());
        let input = body
            .get("input")
            .or_else(|| body.get("messages"))
            .unwrap()
            .as_array()
            .unwrap();
        assert!(input
            .iter()
            .all(|item| item["role"] != "assistant" && item["role"] != "tool"));
        if api == "openai-responses" {
            respond_text(summary, "summary", "PRIVATE SUMMARY: continue this task.");
        } else {
            summary
                .respond_openai_text("summary", "PRIVATE SUMMARY: continue this task.")
                .unwrap();
        }
        let successor = tokio::time::timeout(Duration::from_secs(3), agent.request())
            .await
            .unwrap();
        let body = successor.json().unwrap();
        if api == "openai-responses" {
            assert_eq!(body["max_output_tokens"], 131_072);
        }
        assert!(body.to_string().contains("PRIVATE SUMMARY"));
        assert_eq!(body["tools"].as_array().unwrap().len(), 1);
        assert!(!agent
            .messages(&session)
            .await
            .unwrap()
            .to_string()
            .contains("PRIVATE SUMMARY"));
        if api == "openai-responses" {
            respond_call(successor, "finish", "end", "end", "end", json!({}));
        } else {
            successor
                .respond_openai_calls("finish", [("end", "end", json!({}))])
                .unwrap();
        }
        agent
            .wait_for_state(&session, |state| {
                state.last_turn_outcome == Some(TurnOutcome::Finished)
            })
            .await;
        agent.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
// Contract: docs/zork-agent-architecture.md [PERSIST-01, PROVIDER-01, PROVIDER-02, PROVIDER-03, PROVIDER-04]
async fn real_responses_adapter_preserves_wire_usage_reasoning_and_restart_replay() {
    let started = Instant::now();
    let mut agent = RealAgent::new().unwrap();
    agent
        .install_profile("responses", profile(agent.provider_base_url(), true))
        .unwrap();
    agent
        .install_profile(
            "responses-non-streaming",
            profile(agent.provider_base_url(), false),
        )
        .unwrap();

    let text_session = agent
        .create_configured_session(selection("responses"), None)
        .await
        .unwrap();
    agent.send_mail(&text_session, "say OK").await.unwrap();
    let text_request = agent.request().await;
    assert_eq!(text_request.method, "POST");
    assert_eq!(text_request.path_and_query, "/v1/responses");
    assert_eq!(
        text_request
            .headers
            .get("authorization")
            .map(String::as_str),
        Some("Bearer responses-secret")
    );
    assert_eq!(
        text_request
            .headers
            .get("x-profile-header")
            .map(String::as_str),
        Some("responses-fixture")
    );
    let body = text_request.json().unwrap();
    assert_eq!(body["model"], MODEL);
    assert_eq!(body["stream"], true);
    assert_eq!(body["store"], false);
    assert_eq!(body["parallel_tool_calls"], false);
    assert_eq!(body["max_output_tokens"], 56_000);
    assert_eq!(body["reasoning"]["effort"], "xhigh");
    assert_eq!(body["tools"].as_array().unwrap().len(), 1);
    assert_eq!(body["tools"][0]["name"], "call");
    assert_eq!(
        body["tools"][0]["parameters"]["required"],
        json!(["tool", "arguments"])
    );
    assert!(input(&body)
        .iter()
        .any(|item| item.to_string().contains("say OK")));
    respond_text(text_request, "resp_text", "OK");
    let end_request = agent.request().await;
    let end_body = end_request.json().unwrap();
    assert!(input(&end_body)
        .iter()
        .any(|item| item["id"] == "msg_resp_text"));
    respond_call(
        end_request,
        "resp_end",
        "fc_end",
        "call_end",
        "end",
        json!({}),
    );
    agent
        .wait_for_state(&text_session, |state| {
            state.last_turn_outcome == Some(TurnOutcome::Finished)
        })
        .await;
    assert!(agent
        .history(&text_session, None, 200)
        .unwrap()
        .iter()
        .any(|event| {
            matches!(
                &event.event,
                SessionEvent::StepCompleted { usage: Some(usage), .. }
                    if usage.input_tokens == 123
                        && usage.output_tokens == 45
                        && usage.cached_input_tokens == Some(122)
            )
        }));

    let encrypted_session = agent
        .create_configured_session(selection("responses"), None)
        .await
        .unwrap();
    std::fs::write(
        agent
            .workspace(&encrypted_session)
            .unwrap()
            .join("README.md"),
        "encrypted reasoning replay\n",
    )
    .unwrap();
    agent
        .send_mail(&encrypted_session, "read README with encrypted reasoning")
        .await
        .unwrap();
    let encrypted_raw = respond_reasoning_and_read(
        agent.request().await,
        "resp_encrypted",
        "rs_encrypted",
        "fc_encrypted",
        "call_encrypted",
        true,
    );
    let encrypted_followup = agent.request().await;
    assert_raw_items(
        &encrypted_followup.json().unwrap(),
        &encrypted_raw,
        "call_encrypted",
    );

    let plain_session = agent
        .create_configured_session(selection("responses"), None)
        .await
        .unwrap();
    std::fs::write(
        agent.workspace(&plain_session).unwrap().join("README.md"),
        "plain reasoning replay\n",
    )
    .unwrap();
    agent
        .send_mail(&plain_session, "read README with plain reasoning")
        .await
        .unwrap();
    let plain_raw = respond_reasoning_and_read(
        agent.request().await,
        "resp_plain",
        "rs_plain",
        "fc_plain",
        "call_plain",
        false,
    );
    let plain_followup = agent.request().await;
    assert_raw_items(&plain_followup.json().unwrap(), &plain_raw, "call_plain");

    agent.stop_agent().await;
    drop(encrypted_followup);
    drop(plain_followup);
    agent.start_agent().await.unwrap();
    for _ in 0..2 {
        let replay = agent.request().await;
        let replay_body = replay.json().unwrap();
        if replay_body.to_string().contains("rs_encrypted") {
            assert_raw_items(&replay_body, &encrypted_raw, "call_encrypted");
            respond_call(
                replay,
                "resp_encrypted_end",
                "fc_encrypted_end",
                "call_encrypted_end",
                "end",
                json!({}),
            );
        } else {
            assert!(replay_body.to_string().contains("rs_plain"));
            assert_raw_items(&replay_body, &plain_raw, "call_plain");
            respond_call(
                replay,
                "resp_plain_end",
                "fc_plain_end",
                "call_plain_end",
                "end",
                json!({}),
            );
        }
    }
    for session_id in [&encrypted_session, &plain_session] {
        agent
            .wait_for_state(session_id, |state| {
                state.last_turn_outcome == Some(TurnOutcome::Finished)
            })
            .await;
        assert!(agent
            .history(session_id, None, 200)
            .unwrap()
            .iter()
            .any(|event| {
                matches!(
                    &event.event,
                    SessionEvent::StepCompleted {
                        provider_context: Some(context),
                        ..
                    } if context.output_items.iter().any(|item| {
                        item["type"] == "reasoning"
                    })
                )
            }));
    }

    let non_streaming_session = agent
        .create_configured_session(selection("responses-non-streaming"), None)
        .await
        .unwrap();
    std::fs::write(
        agent
            .workspace(&non_streaming_session)
            .unwrap()
            .join("README.md"),
        "non-streaming reasoning\n",
    )
    .unwrap();
    agent
        .send_mail(&non_streaming_session, "read README without streaming")
        .await
        .unwrap();
    let non_streaming_request = agent.request().await;
    let non_streaming_body = non_streaming_request.json().unwrap();
    assert!(non_streaming_body.get("stream").is_none());
    assert_eq!(non_streaming_body["store"], false);
    let non_streaming_raw = vec![
        reasoning_item("rs_non_streaming", true),
        response_call_item(
            "fc_non_streaming",
            "call_non_streaming",
            "file.read",
            json!({"path": "README.md"}),
        ),
    ];
    non_streaming_request
        .respond_json(
            StatusCode::OK,
            non_streaming_response(non_streaming_raw.clone(), 200, 60),
        )
        .unwrap();
    let non_streaming_followup = agent.request().await;
    let non_streaming_followup_body = non_streaming_followup.json().unwrap();
    assert!(non_streaming_followup_body.get("stream").is_none());
    assert_raw_items(
        &non_streaming_followup_body,
        &non_streaming_raw,
        "call_non_streaming",
    );
    non_streaming_followup
        .respond_json(
            StatusCode::OK,
            non_streaming_response(
                vec![response_call_item(
                    "fc_non_streaming_end",
                    "call_non_streaming_end",
                    "end",
                    json!({}),
                )],
                10,
                2,
            ),
        )
        .unwrap();
    agent
        .wait_for_state(&non_streaming_session, |state| {
            state.last_turn_outcome == Some(TurnOutcome::Finished)
        })
        .await;
    assert!(agent
        .history(&non_streaming_session, None, 200)
        .unwrap()
        .iter()
        .any(|event| matches!(
            &event.event,
            SessionEvent::StepCompleted { usage: Some(usage), .. }
                if usage.input_tokens == 200 && usage.output_tokens == 60
        )));

    agent.shutdown().await;
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "Responses real lifecycle took {:?}",
        started.elapsed()
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
// Contract: docs/zork-agent-architecture.md [PROVIDER-01, RETRY-01]
async fn streaming_and_complete_responses_stay_on_one_request_across_thirty_silent_seconds() {
    let _io_guard = PausedTimeIoGuard::start();
    let router = Arc::new(ProviderRouter::new());
    let mut streaming_provider = ControlledHttpProvider::start(4).unwrap();
    let (deltas, mut observed_deltas) = tokio::sync::mpsc::unbounded_channel();
    let streaming_task = tokio::spawn({
        let router = router.clone();
        let request = provider_request("responses", Arc::new(DeltaObserver { deltas }));
        let execution = provider_execution(streaming_provider.base_url(), "responses", true);
        async move { router.complete(&request, execution).await }
    });
    let stream = streaming_provider.request().await.begin_sse(16).unwrap();
    let response_events = text_events("resp_silent_stream", "stream completed");
    for event in &response_events[..3] {
        stream.send_json(event.clone()).await.unwrap();
    }
    assert_eq!(observed_deltas.recv().await.unwrap(), "stream completed");

    advance_real_transport(Duration::from_secs(31)).await;
    assert!(!streaming_task.is_finished());

    for event in &response_events[3..] {
        stream.send_json(event.clone()).await.unwrap();
    }
    stream.finish().await.unwrap();
    let streaming_outcome = streaming_task.await.unwrap().unwrap();
    assert_eq!(streaming_outcome.text, "stream completed");
    streaming_provider.shutdown().await;

    let mut complete_provider = ControlledHttpProvider::start(4).unwrap();
    let complete_task = tokio::spawn({
        let request = provider_request("responses-non-streaming", Arc::new(SilentStreamObserver));
        let execution = provider_execution(
            complete_provider.base_url(),
            "responses-non-streaming",
            false,
        );
        async move { router.complete(&request, execution).await }
    });
    let complete_request = complete_provider.request().await;
    advance_real_transport(Duration::from_secs(31)).await;
    assert!(!complete_task.is_finished());
    complete_request
        .respond_json(
            StatusCode::OK,
            non_streaming_response(
                vec![json!({
                    "id": "msg_silent_complete",
                    "type": "message",
                    "status": "completed",
                    "role": "assistant",
                    "content": [{
                        "type": "output_text",
                        "text": "complete response",
                        "annotations": []
                    }]
                })],
                10,
                2,
            ),
        )
        .unwrap();
    let complete_outcome = complete_task.await.unwrap().unwrap();
    assert_eq!(complete_outcome.text, "complete response");
    complete_provider.shutdown().await;
}

#[tokio::test]
// Contract: docs/zork-agent-architecture.md [PROVIDER-01, RETRY-02]
async fn responses_eof_without_a_terminal_event_is_a_diagnostic_provider_failure() {
    let router = ProviderRouter::new();
    let mut provider = ControlledHttpProvider::start(4).unwrap();
    let request = provider_request("responses", Arc::new(SilentStreamObserver));
    let execution = provider_execution(provider.base_url(), "responses", true);
    let task = tokio::spawn(async move { router.complete(&request, execution).await });

    let stream = provider.request().await.begin_sse(16).unwrap();
    for event in &text_events("resp_incomplete", "partial")[..3] {
        stream.send_json(event.clone()).await.unwrap();
    }
    stream.finish().await.unwrap();

    let ModelError::ProviderFailed(failure) = task.await.unwrap().unwrap_err() else {
        panic!("expected provider failure");
    };
    assert_eq!(failure.stage, "provider.stream.finish");
    assert!(failure.message.contains("terminal response event"));
    let diagnostics = failure.provider_input.expect("provider input diagnostics");
    assert_eq!(diagnostics.response_id.as_deref(), Some("resp_incomplete"));
    provider.shutdown().await;
}
