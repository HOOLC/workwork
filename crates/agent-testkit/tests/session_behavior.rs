use serde_json::json;
use std::{sync::Arc, time::Duration};
use zork_agent::session::events::{
    AutoWaitEndReason, SessionEvent, StepInterruptionReason, ToolDelivery, ToolOutcome, TurnOutcome,
};
use zork_agent::session::model::{
    ModelError, ModelOutcome, ModelTokenUsage, ProviderFailure, TOOL_INTERRUPTED_MESSAGE,
};
use zork_agent::session::service::ServiceOptions;
use zork_agent::session::state::GenerationEntry;
use zork_agent::session::supervisor::PublicSlotStatus;
use zork_agent::session::tools::{ToolContract, ToolVersion};
use zork_agent::session::wire::{
    ProviderContext, ProviderInputDiagnostics, ProviderInputMode, ProviderToolCall,
    SessionSelection, TranscriptRole,
};
use zork_agent_testkit::TestWorld;

fn selection() -> SessionSelection {
    SessionSelection {
        profile_id: "test-profile".into(),
        model: "test-model".into(),
        thinking: "medium".into(),
    }
}

async fn session(world: &TestWorld, workspace: &str) -> String {
    world
        .create_session(selection(), None, workspace)
        .await
        .expect("create session")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
// Contract: docs/zork-agent-architecture.md [SUPERVISOR-03]
async fn concurrent_inspection_of_an_idle_session_does_not_wait_for_new_input() {
    let mut world = TestWorld::new();
    let mut inspections = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let session_id = session(&world, "/virtual/idle-inspection").await;
        let service = world.service_handle();
        inspections.spawn(async move {
            for _ in 0..100 {
                let state = service.state(&session_id).await.unwrap();
                assert_eq!(state.session_id, session_id);
                assert!(state.active_turn.is_none());
                tokio::task::yield_now().await;
            }
        });
    }
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(result) = inspections.join_next().await {
            result.unwrap();
        }
    })
    .await
    .expect("idle session inspections must complete without another input");
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
// Contract: docs/zork-agent-architecture.md [TOOL-01, TURN-01]
async fn one_provider_call_definition_continues_until_explicit_end() {
    let mut world = TestWorld::new();
    let session_id = session(&world, "/virtual/call-end").await;

    world.send_mail(&session_id, "do the work").await.unwrap();
    let first = world.request().await;
    assert_eq!(
        first
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        vec!["call"]
    );
    assert!(first.tools[0].input_schema["properties"]["tool"]
        .get("enum")
        .is_none());
    assert!(first.transcript.iter().any(|message| {
        message.role == TranscriptRole::User && message.content.as_ref() == "do the work"
    }));
    first.respond_text("work completed").unwrap();

    let second = world.request().await;
    assert!(second.transcript.iter().any(|message| {
        message.role == TranscriptRole::Assistant && message.content.as_ref() == "work completed"
    }));
    second
        .respond_call("provider-call-2", "end", json!({}))
        .unwrap();

    world
        .wait_for_state(&session_id, |state| {
            state.last_turn_outcome == Some(TurnOutcome::Finished)
        })
        .await;
    let events = world.events(&session_id);
    assert!(events
        .iter()
        .any(|event| matches!(event.event, SessionEvent::InputAppended { .. })));
    assert!(events
        .iter()
        .any(|event| matches!(event.event, SessionEvent::TurnStarted { .. })));
    assert!(events
        .iter()
        .any(|event| matches!(event.event, SessionEvent::StepCompleted { .. })));
    assert!(events
        .iter()
        .any(|event| matches!(event.event, SessionEvent::ToolResult { .. })));
    assert!(events.iter().any(|event| matches!(
        event.event,
        SessionEvent::TurnFinished {
            outcome: TurnOutcome::Finished,
            ..
        }
    )));
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
// Contract: docs/zork-agent-architecture.md [TURN-01]
async fn end_ignores_arguments_without_end_semantics() {
    let mut world = TestWorld::new();
    let session_id = session(&world, "/virtual/end-extra-arguments").await;

    world.send_mail(&session_id, "do the work").await.unwrap();
    world
        .request()
        .await
        .respond_call(
            "provider-end-with-reason",
            "end",
            json!({"reason": "Implemented the requested change."}),
        )
        .unwrap();

    world
        .wait_for_state(&session_id, |state| {
            state.last_turn_outcome == Some(TurnOutcome::Finished)
        })
        .await;
    assert!(world.events(&session_id).iter().any(|event| matches!(
        &event.event,
        SessionEvent::ToolResult { result }
            if result.tool == "end" && result.outcome == ToolOutcome::Succeeded
    )));
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
// Contract: docs/zork-agent-architecture.md [TURN-01]
async fn failed_end_is_returned_to_the_agent_instead_of_finishing_the_turn() {
    let mut world = TestWorld::new();
    let session_id = session(&world, "/virtual/failed-end").await;

    world.send_mail(&session_id, "do the work").await.unwrap();
    world
        .request()
        .await
        .respond_call(
            "provider-invalid-end",
            "end",
            json!({"acknowledge_outstanding": "yes"}),
        )
        .unwrap();

    let retry = world.request().await;
    assert!(retry.transcript.iter().any(|message| {
        message.role == TranscriptRole::Tool
            && message.tool_call_id.as_deref() == Some("provider-invalid-end")
            && message.content.contains("Invalid tool arguments")
    }));
    assert!(world
        .state(&session_id)
        .await
        .unwrap()
        .active_turn
        .is_some());

    retry
        .respond_call("provider-valid-end", "end", json!({}))
        .unwrap();
    world
        .wait_for_state(&session_id, |state| {
            state.last_turn_outcome == Some(TurnOutcome::Finished)
        })
        .await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
// Contract: docs/zork-agent-architecture.md [EVENT-05, TOOL-01, TOOL-10]
async fn malformed_dynamic_call_preserves_the_provider_outcome_and_returns_a_tool_error() {
    let mut options = ServiceOptions::default();
    options.runner.provider_retry_base = Duration::ZERO;
    options.runner.provider_retry_max = Duration::ZERO;
    let mut world = TestWorld::with_options(options);
    let session_id = session(&world, "/virtual/malformed-dynamic-call").await;

    world
        .send_mail(&session_id, "finish the turn")
        .await
        .unwrap();
    world
        .request()
        .await
        .respond(Ok(ModelOutcome {
            text: "provider response survives validation".into(),
            tool_calls: vec![ProviderToolCall {
                tool_call_id: "provider-malformed".into(),
                tool_name: "call".into(),
                arguments: json!({
                    "tool": "end",
                    "parameters": {"acknowledge_outstanding": true}
                }),
            }],
            provider_context: Some(ProviderContext {
                profile_id: "test-profile".into(),
                provider: "controlled".into(),
                model: "test-model".into(),
                api: "responses".into(),
                output_items: Arc::new(vec![json!({"id": "provider-malformed"})]),
            }),
            usage: Some(ModelTokenUsage {
                input_tokens: 11,
                cached_input_tokens: Some(3),
                output_tokens: 7,
                output_reasoning_tokens: Some(2),
                output_text_tokens: Some(5),
            }),
            provider_input: Some(Box::new(ProviderInputDiagnostics {
                mode: ProviderInputMode::Delta,
                logical_input_items: 9,
                sent_input_items: 2,
                previous_response_id: Some("response-before".into()),
                response_id: Some("response-after".into()),
            })),
        }))
        .unwrap();

    let correction = world.request().await;
    assert!(correction.transcript.iter().any(|message| {
        message.role == TranscriptRole::Assistant
            && message.content.as_ref() == "provider response survives validation"
            && message
                .tool_calls
                .iter()
                .any(|call| call.tool_call_id == "provider-malformed")
    }));
    assert!(correction.transcript.iter().any(|message| {
        message.role == TranscriptRole::Tool
            && message.tool_call_id.as_deref() == Some("provider-malformed")
            && message.content.contains("Invalid provider call")
    }));
    correction
        .respond_call("provider-end-after-malformed", "end", json!({}))
        .unwrap();
    world
        .wait_for_state(&session_id, |state| {
            state.last_turn_outcome == Some(TurnOutcome::Finished)
        })
        .await;

    let events = world.events(&session_id);
    assert!(!events
        .iter()
        .any(|event| matches!(event.event, SessionEvent::StepFailed { .. })));
    let completed = events
        .iter()
        .find(|event| {
            matches!(
                &event.event,
                SessionEvent::StepCompleted { provider_calls, .. }
                    if provider_calls.iter().any(|call| call.tool_call_id == "provider-malformed")
            )
        })
        .expect("the malformed provider outcome must remain a completed step");
    let completed = serde_json::to_value(&completed.event).unwrap();
    assert_eq!(completed["usage"]["input_tokens"], 11);
    assert_eq!(completed["provider_context"]["provider"], "controlled");
    assert_eq!(completed["provider_input"]["sent_input_items"], 2);
    assert_eq!(
        completed["invocations"][0]["arguments"]["parameters"]["acknowledge_outstanding"],
        true
    );
    assert!(completed["invocations"][0]["arguments"]
        .get("arguments")
        .is_none());
    assert!(completed["invocations"][0]["rejection"]
        .as_str()
        .is_some_and(|reason| reason.contains("missing field `arguments`")));
    assert!(events.iter().any(|event| matches!(
        &event.event,
        SessionEvent::ToolResult { result }
            if result.tool == "end"
                && result.outcome == ToolOutcome::Failed
                && result.data["error"]
                    .as_str()
                    .is_some_and(|error| error.contains("Invalid provider call"))
    )));
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
// Contract: docs/zork-agent-architecture.md [MAILBOX-01]
async fn mailbox_input_arriving_during_a_provider_step_is_sent_next() {
    let mut world = TestWorld::new();
    let session_id = session(&world, "/virtual/inflight-mailbox").await;

    world.send_mail(&session_id, "first").await.unwrap();
    let first = world.request().await;
    world.send_mail(&session_id, "second").await.unwrap();
    first.respond_text("first complete").unwrap();

    let second = world.request().await;
    let users = second
        .transcript
        .iter()
        .filter(|message| message.role == TranscriptRole::User)
        .map(|message| message.content.as_ref())
        .collect::<Vec<_>>();
    assert_eq!(users, vec!["first", "second"]);
    second
        .respond_call("provider-call-2", "end", json!({}))
        .unwrap();
    world
        .wait_for_state(&session_id, |state| {
            state.last_turn_outcome == Some(TurnOutcome::Finished)
        })
        .await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
// Contract: docs/zork-agent-architecture.md [WAIT-01, EVENT-05]
async fn one_auto_wait_delivers_every_result_from_a_completed_tool_batch() {
    let mut world = TestWorld::new();
    let workspace = "/virtual/tool-batch";
    let session_id = session(&world, workspace).await;

    world
        .send_mail(&session_id, "write both files")
        .await
        .unwrap();
    world
        .request()
        .await
        .respond_calls([
            (
                "provider-call-1-0",
                "file.write",
                json!({"path": "one.txt", "content": "one"}),
            ),
            (
                "provider-call-1-1",
                "file.write",
                json!({"path": "two.txt", "content": "two"}),
            ),
        ])
        .unwrap();

    let second = world.request().await;
    let mut tool_call_ids = second
        .transcript
        .iter()
        .filter(|message| message.role == TranscriptRole::Tool)
        .filter_map(|message| message.tool_call_id.as_deref())
        .collect::<Vec<_>>();
    tool_call_ids.sort_unstable();
    assert_eq!(
        tool_call_ids,
        vec!["provider-call-1-0", "provider-call-1-1"]
    );
    assert_eq!(
        world.files.read_text(format!("{workspace}/one.txt")),
        Some("one".into())
    );
    assert_eq!(
        world.files.read_text(format!("{workspace}/two.txt")),
        Some("two".into())
    );
    let completed_auto_waits = world
        .events(&session_id)
        .iter()
        .filter(|event| {
            matches!(
                event.event,
                SessionEvent::AutoWaitEnded {
                    reason: AutoWaitEndReason::BatchCompleted,
                    ..
                }
            )
        })
        .count();
    assert_eq!(completed_auto_waits, 1);

    second
        .respond_call("provider-call-2", "end", json!({}))
        .unwrap();
    world
        .wait_for_state(&session_id, |state| {
            state.last_turn_outcome == Some(TurnOutcome::Finished)
        })
        .await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
// Contract: docs/zork-agent-architecture.md [TOOL-11, WAIT-01, EVENT-05]
async fn concurrent_tool_results_are_not_mistaken_for_interrupted_executions() {
    let mut options = ServiceOptions::default();
    options.runner.tool_result_capacity = 1;
    let mut world = TestWorld::with_options(options);
    let session_id = session(&world, "/virtual/result-backpressure").await;
    world.send_mail(&session_id, "write files").await.unwrap();
    let mut request = world.request().await;
    for batch in 0..16 {
        request
            .respond_calls((0..16).map(|index| {
                (
                    format!("write-{batch}-{index}"),
                    "file.write",
                    json!({"path": format!("{batch}-{index}.txt"), "content": "ok"}),
                )
            }))
            .unwrap();
        request = world.request().await;
        let results = world
            .events(&session_id)
            .into_iter()
            .filter_map(|envelope| match envelope.event {
                SessionEvent::ToolResult { result } => Some(result),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(results.len(), (batch + 1) * 16);
        assert!(results
            .iter()
            .all(|result| result.outcome == ToolOutcome::Succeeded));
    }
    request
        .respond_call("end-after-backpressure", "end", json!({}))
        .unwrap();
    world
        .wait_for_state(&session_id, |state| {
            state.last_turn_outcome == Some(TurnOutcome::Finished)
        })
        .await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
// Contract: docs/zork-agent-architecture.md [WAIT-02]
async fn new_mail_ends_an_explicit_wait_and_starts_the_next_step() {
    let mut world = TestWorld::new();
    let session_id = session(&world, "/virtual/explicit-wait").await;

    world.send_mail(&session_id, "wait for more").await.unwrap();
    world
        .request()
        .await
        .respond_call(
            "provider-call-1",
            "wait",
            json!({"seconds": 30, "reason": "more input"}),
        )
        .unwrap();
    world
        .wait_for_state(&session_id, |state| state.wait_deadline.is_some())
        .await;

    world.send_mail(&session_id, "continue now").await.unwrap();
    let second = world.request().await;
    assert!(second.transcript.iter().any(|message| {
        message.role == TranscriptRole::User && message.content.as_ref() == "continue now"
    }));
    second
        .respond_call("provider-call-2", "end", json!({}))
        .unwrap();
    world
        .wait_for_state(&session_id, |state| {
            state.last_turn_outcome == Some(TurnOutcome::Finished)
        })
        .await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
// Contract: docs/zork-agent-architecture.md [DEADLINE-01]
async fn explicit_wait_deadline_is_rebuilt_after_restart_and_reached_once() {
    let mut world = TestWorld::new();
    let session_id = session(&world, "/virtual/rebuilt-wait-deadline").await;

    world
        .send_mail(&session_id, "wait across restart")
        .await
        .unwrap();
    world
        .request()
        .await
        .respond_call(
            "provider-rebuilt-wait",
            "wait",
            json!({"seconds": 10, "reason": "restart coverage"}),
        )
        .unwrap();
    let waiting = world
        .wait_for_state(&session_id, |state| state.wait_deadline.is_some())
        .await;
    let deadline_ms = waiting.wait_deadline.unwrap().deadline_ms;

    world.restart().await.unwrap();
    wait_for_clock_deadline(&world, deadline_ms).await;
    world.clock.advance_to(deadline_ms);

    let resumed = world.request().await;
    assert!(resumed.transcript.iter().any(|message| {
        message.content.contains("reached its deadline")
            && message.content.contains(&deadline_ms.to_string())
    }));
    assert_eq!(
        world
            .events(&session_id)
            .iter()
            .filter(|event| matches!(event.event, SessionEvent::DeadlineReached { .. }))
            .count(),
        1
    );
    resumed
        .respond_call("provider-rebuilt-wait-end", "end", json!({}))
        .unwrap();
    world
        .wait_for_state(&session_id, |state| {
            state.last_turn_outcome == Some(TurnOutcome::Finished)
        })
        .await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
// Contract: docs/zork-agent-architecture.md [CANCEL-01]
async fn cancelling_an_active_turn_preserves_the_session_for_a_new_turn() {
    let mut world = TestWorld::new();
    let session_id = session(&world, "/virtual/cancel-turn").await;

    world.send_mail(&session_id, "first turn").await.unwrap();
    let cancelled_request = world.request().await;
    world.cancel(&session_id).await.unwrap();
    world
        .wait_for_state(&session_id, |state| {
            state.last_turn_outcome == Some(TurnOutcome::Cancelled)
        })
        .await;
    drop(cancelled_request);

    world.send_mail(&session_id, "second turn").await.unwrap();
    let second = world.request().await;
    assert!(second.transcript.iter().any(|message| {
        message.role == TranscriptRole::User && message.content.as_ref() == "second turn"
    }));
    second
        .respond_call("provider-call-2", "end", json!({}))
        .unwrap();
    world
        .wait_for_state(&session_id, |state| {
            state.last_turn_outcome == Some(TurnOutcome::Finished)
        })
        .await;
    world.shutdown().await;
}

fn controlled_tool(name: &str) -> ToolContract {
    ToolContract {
        name: name.into(),
        version: ToolVersion::new("test-1").unwrap(),
        initial_description: format!("Run controlled {name} work."),
        detailed_description: format!("The test controller completes {name} work."),
        input_schema: json!({
            "type": "object",
            "properties": {"value": {"type": "string"}},
            "required": ["value"],
            "additionalProperties": false
        }),
    }
}

#[tokio::test(flavor = "multi_thread")]
// Contract: docs/zork-agent-architecture.md [TURN-01, PROJECTION-03]
async fn end_with_unfinished_work_reports_it_then_finishes_after_the_result() {
    let mut world = TestWorld::new();
    let mut slow = world.install_tool(controlled_tool("test.slow")).unwrap();
    let session_id = session(&world, "/virtual/end-outstanding").await;

    world
        .send_mail(&session_id, "start the work")
        .await
        .unwrap();
    world
        .request()
        .await
        .respond_calls([
            ("provider-slow", "test.slow", json!({"value": "controlled"})),
            ("provider-end", "end", json!({})),
        ])
        .unwrap();
    let pending = slow.request().await;
    world
        .wait_for_state(&session_id, |state| {
            state.auto_wait.is_some()
                && state
                    .pending_tools
                    .values()
                    .any(|pending| pending.invocation.tool == "end" && pending.result.is_some())
        })
        .await;

    world
        .send_mail(&session_id, "continue while it is pending")
        .await
        .unwrap();
    let outstanding = world.request().await;
    assert!(outstanding.transcript.iter().any(|message| {
        message.content.contains("test.slow")
            && message.content.contains("has not returned a final result")
    }));
    pending
        .succeed(json!({
            "message": "controlled work completed",
            "value": "controlled",
        }))
        .unwrap();
    world
        .wait_for_state(&session_id, |state| {
            state
                .pending_tools
                .values()
                .any(|pending| pending.invocation.tool == "test.slow" && pending.result.is_some())
        })
        .await;
    outstanding
        .respond_text("I will use the completed result.")
        .unwrap();

    let completed = world.request().await;
    assert!(completed
        .transcript
        .iter()
        .any(|message| message.content.contains("controlled work completed")));
    completed
        .respond_call("provider-final-end", "end", json!({}))
        .unwrap();
    world
        .wait_for_state(&session_id, |state| {
            state.last_turn_outcome == Some(TurnOutcome::Finished)
        })
        .await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
// Contract: docs/zork-agent-architecture.md [STATE-01, RECOVERY-01]
async fn restart_resumes_an_active_text_turn_and_reports_the_interrupted_step() {
    let mut world = TestWorld::new();
    let session_id = session(&world, "/virtual/active-restart").await;

    world
        .send_mail(&session_id, "continue across restart")
        .await
        .unwrap();
    world
        .request()
        .await
        .respond_text("durable text before restart")
        .unwrap();
    let interrupted_request = world.request().await;

    world.restart().await.unwrap();
    drop(interrupted_request);
    let resumed = world.request().await;
    assert!(resumed.transcript.iter().any(|message| {
        message.role == TranscriptRole::Assistant
            && message.content.as_ref() == "durable text before restart"
    }));
    assert!(resumed.transcript.iter().any(|message| {
        message
            .content
            .contains("was interrupted during runtime recovery")
    }));
    resumed
        .respond_call("provider-end-after-restart", "end", json!({}))
        .unwrap();
    world
        .wait_for_state(&session_id, |state| {
            state.last_turn_outcome == Some(TurnOutcome::Finished)
        })
        .await;
    assert!(world.events(&session_id).iter().any(|event| matches!(
        event.event,
        SessionEvent::StepInterrupted {
            reason: StepInterruptionReason::Recovery,
            ..
        }
    )));
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
// Contract: docs/zork-agent-architecture.md [RETRY-01, MAILBOX-01]
async fn retrying_a_provider_step_includes_mail_received_during_the_failed_attempt() {
    let mut options = ServiceOptions::default();
    options.runner.provider_retry_base = Duration::from_millis(1);
    options.runner.provider_retry_max = Duration::from_millis(1);
    let mut world = TestWorld::with_options(options);
    let session_id = session(&world, "/virtual/provider-retry-mail").await;

    world
        .send_mail(&session_id, "before failure")
        .await
        .unwrap();
    let failed = world.request().await;
    world
        .send_mail(&session_id, "arrived during failed request")
        .await
        .unwrap();
    failed
        .fail_provider("controlled.provider", true, "temporary provider outage")
        .unwrap();
    world
        .wait_for_state(&session_id, |state| {
            state
                .active_turn
                .as_ref()
                .is_some_and(|turn| turn.consecutive_provider_failures == 1)
        })
        .await;
    wait_for_clock_timer(&world).await;
    world.clock.advance(Duration::from_millis(1));

    let retried = world.request().await;
    let users = retried
        .transcript
        .iter()
        .filter(|message| message.role == TranscriptRole::User)
        .map(|message| message.content.as_ref())
        .collect::<Vec<_>>();
    assert_eq!(users.len(), 3);
    assert_eq!(users[0], "before failure");
    assert!(users[1].contains("temporary provider outage"));
    assert_eq!(users[2], "arrived during failed request");
    assert!(retried
        .transcript
        .iter()
        .any(|message| message.content.contains("temporary provider outage")));
    retried
        .respond_call("provider-end-after-retry", "end", json!({}))
        .unwrap();
    world
        .wait_for_state(&session_id, |state| {
            state.last_turn_outcome == Some(TurnOutcome::Finished)
        })
        .await;
    assert_eq!(
        world
            .events(&session_id)
            .iter()
            .filter(|event| matches!(event.event, SessionEvent::StepFailed { .. }))
            .count(),
        1
    );
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
// Contract: docs/zork-agent-architecture.md [RETRY-02]
async fn a_permanent_provider_failure_waits_for_new_mail_then_the_session_recovers() {
    let mut world = TestWorld::new();
    let session_id = session(&world, "/virtual/permanent-provider-failure").await;

    world.send_mail(&session_id, "first attempt").await.unwrap();
    world
        .request()
        .await
        .respond(Err(ModelError::ProviderFailed(ProviderFailure {
            stage: "controlled.authentication",
            retryable: false,
            status_code: Some(401),
            provider_code: Some("login_required".into()),
            request_id: Some("request-authentication".into()),
            message: "provider login is required".into(),
            provider_input: Some(Box::new(ProviderInputDiagnostics {
                mode: ProviderInputMode::Full,
                logical_input_items: 4,
                sent_input_items: 4,
                previous_response_id: None,
                response_id: None,
            })),
            usage: Some(Box::new(zork_agent::session::model::ModelTokenUsage {
                input_tokens: 321,
                cached_input_tokens: Some(123),
                output_tokens: 45,
                output_reasoning_tokens: Some(34),
                output_text_tokens: Some(11),
            })),
        })))
        .unwrap();
    world
        .wait_for_state(&session_id, |state| {
            state.last_turn_outcome == Some(TurnOutcome::Failed) && state.active_turn.is_none()
        })
        .await;
    let failed = world
        .events(&session_id)
        .into_iter()
        .find(|event| matches!(event.event, SessionEvent::StepFailed { .. }))
        .expect("provider failure must be durable");
    let failed = serde_json::to_value(failed.event).unwrap();
    assert_eq!(failed["error"]["provider_input"]["sent_input_items"], 4);
    assert_eq!(failed["error"]["usage"]["input_tokens"], 321);
    assert_eq!(failed["error"]["usage"]["cached_input_tokens"], 123);
    assert_eq!(failed["error"]["usage"]["output_tokens"], 45);
    assert_eq!(
        world
            .state(&session_id)
            .await
            .unwrap()
            .token_anchor
            .map(|anchor| anchor.input_tokens),
        Some(321)
    );

    world
        .send_mail(&session_id, "login is fixed; continue")
        .await
        .unwrap();
    let recovered = world.request().await;
    assert!(recovered
        .transcript
        .iter()
        .any(|message| message.content.contains("provider login is required")));
    assert!(recovered
        .transcript
        .iter()
        .any(|message| { message.content.as_ref() == "login is fixed; continue" }));
    recovered
        .respond_call("provider-end-after-login", "end", json!({}))
        .unwrap();
    world
        .wait_for_state(&session_id, |state| {
            state.last_turn_outcome == Some(TurnOutcome::Finished)
        })
        .await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
// Contract: docs/zork-agent-architecture.md [RETRY-02]
async fn ten_retryable_provider_failures_end_the_turn_and_new_mail_recovers_the_session() {
    let mut options = ServiceOptions::default();
    options.runner.provider_retry_base = Duration::from_millis(1);
    options.runner.provider_retry_max = Duration::from_millis(1);
    let mut world = TestWorld::with_options(options);
    let session_id = session(&world, "/virtual/provider-retry-limit").await;

    world
        .send_mail(&session_id, "begin retryable work")
        .await
        .unwrap();
    for attempt in 1..=10 {
        world
            .request()
            .await
            .fail_provider(
                "controlled.capacity",
                true,
                format!("temporary capacity failure {attempt}"),
            )
            .unwrap();

        if attempt < 10 {
            world
                .wait_for_state(&session_id, |state| {
                    state
                        .active_turn
                        .as_ref()
                        .is_some_and(|turn| turn.consecutive_provider_failures == attempt)
                })
                .await;
            wait_for_clock_timer(&world).await;
            world.clock.advance(Duration::from_millis(1));
        }
    }

    world
        .wait_for_state(&session_id, |state| {
            state.last_turn_outcome == Some(TurnOutcome::Failed) && state.active_turn.is_none()
        })
        .await;
    world
        .send_mail(&session_id, "capacity recovered; continue")
        .await
        .unwrap();
    let recovered = world.request().await;
    assert!(recovered
        .transcript
        .iter()
        .any(|message| message.content.contains("temporary capacity failure 10")));
    assert!(recovered
        .transcript
        .iter()
        .any(|message| { message.content.as_ref() == "capacity recovered; continue" }));
    recovered
        .respond_call("provider-end-after-retry-limit", "end", json!({}))
        .unwrap();
    world
        .wait_for_state(&session_id, |state| {
            state.last_turn_outcome == Some(TurnOutcome::Finished)
        })
        .await;
    assert_eq!(
        world
            .events(&session_id)
            .iter()
            .filter(|event| matches!(event.event, SessionEvent::StepFailed { .. }))
            .count(),
        10
    );
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
// Contract: docs/zork-agent-architecture.md [RECOVERY-02, PROJECTION-03]
async fn restart_preserves_a_recorded_tool_result_and_reports_the_unfinished_peer() {
    let mut world = TestWorld::new();
    let mut controlled = world
        .install_tool(controlled_tool("test.controlled"))
        .unwrap();
    let session_id = session(&world, "/virtual/tool-recovery").await;

    world
        .send_mail(&session_id, "run both operations")
        .await
        .unwrap();
    world
        .request()
        .await
        .respond_calls([
            (
                "provider-first",
                "test.controlled",
                json!({"value": "first"}),
            ),
            (
                "provider-second",
                "test.controlled",
                json!({"value": "second"}),
            ),
            (
                "provider-third",
                "test.controlled",
                json!({"value": "third"}),
            ),
        ])
        .unwrap();
    let mut requests = vec![
        controlled.request().await,
        controlled.request().await,
        controlled.request().await,
    ];
    let first_index = requests
        .iter()
        .position(|request| request.arguments["value"] == "first")
        .unwrap();
    let first = requests.swap_remove(first_index);
    first
        .succeed(json!({"message": "first result is durable", "value": "first"}))
        .unwrap();
    world
        .wait_for_state(&session_id, |state| {
            state.pending_tools.values().any(|pending| {
                pending.invocation.arguments["value"] == "first"
                    && pending
                        .result
                        .as_ref()
                        .is_some_and(|result| result.outcome == ToolOutcome::Succeeded)
            })
        })
        .await;

    world.restart().await.unwrap();
    drop(requests);
    let resumed = world.request().await;
    let tool_messages = resumed
        .transcript
        .iter()
        .filter(|message| message.role == TranscriptRole::Tool)
        .map(|message| message.content.as_ref())
        .collect::<Vec<_>>();
    assert_eq!(tool_messages.len(), 3);
    assert!(tool_messages
        .iter()
        .any(|message| message.contains("first result is durable")));
    assert_eq!(
        tool_messages
            .iter()
            .filter(|message| message.contains(TOOL_INTERRUPTED_MESSAGE))
            .count(),
        2
    );
    resumed
        .respond_call("provider-end-after-tool-recovery", "end", json!({}))
        .unwrap();
    world
        .wait_for_state(&session_id, |state| {
            state.last_turn_outcome == Some(TurnOutcome::Finished)
        })
        .await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
// Contract: docs/zork-agent-architecture.md [TOOL-10, PROJECTION-02]
async fn controlled_tools_start_together_and_project_results_in_declaration_order() {
    let mut world = TestWorld::new();
    let mut first_tool = world.install_tool(controlled_tool("test.first")).unwrap();
    let mut second_tool = world.install_tool(controlled_tool("test.second")).unwrap();
    let session_id = session(&world, "/virtual/tool-order").await;

    world
        .send_mail(&session_id, "run independent work")
        .await
        .unwrap();
    world
        .request()
        .await
        .respond_calls([
            ("provider-first", "test.first", json!({"value": "first"})),
            ("provider-second", "test.second", json!({"value": "second"})),
        ])
        .unwrap();
    let first = first_tool.request().await;
    let second = second_tool.request().await;
    second
        .succeed(json!({
            "message": "second completed first",
            "value": "second",
        }))
        .unwrap();
    world
        .wait_for_state(&session_id, |state| {
            state
                .pending_tools
                .values()
                .any(|pending| pending.invocation.tool == "test.second" && pending.result.is_some())
        })
        .await;
    first
        .succeed(json!({
            "message": "first completed second",
            "value": "first",
        }))
        .unwrap();

    let projected = world.request().await;
    let tool_call_ids = projected
        .transcript
        .iter()
        .filter(|message| message.role == TranscriptRole::Tool)
        .filter_map(|message| message.tool_call_id.as_deref())
        .collect::<Vec<_>>();
    assert_eq!(tool_call_ids, vec!["provider-first", "provider-second"]);
    projected
        .respond_call("provider-end-after-order", "end", json!({}))
        .unwrap();
    world
        .wait_for_state(&session_id, |state| {
            state.last_turn_outcome == Some(TurnOutcome::Finished)
        })
        .await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
// Contract: docs/zork-agent-architecture.md [WAIT-01, PROJECTION-03]
async fn auto_wait_timeout_projects_pending_work_then_delivers_the_late_result() {
    let mut options = ServiceOptions::default();
    options.runner.auto_wait = Duration::from_millis(10);
    let mut world = TestWorld::with_options(options);
    let mut slow = world.install_tool(controlled_tool("test.late")).unwrap();
    let session_id = session(&world, "/virtual/auto-wait-timeout").await;

    world
        .send_mail(&session_id, "start delayed work")
        .await
        .unwrap();
    world
        .request()
        .await
        .respond_call("provider-late", "test.late", json!({"value": "eventual"}))
        .unwrap();
    let pending = slow.request().await;
    world
        .wait_for_state(&session_id, |state| state.auto_wait.is_some())
        .await;
    wait_for_clock_timer(&world).await;
    world.clock.advance(Duration::from_millis(10));

    let after_timeout = world.request().await;
    assert!(after_timeout.transcript.iter().any(|message| {
        message.role == TranscriptRole::Tool
            && message.tool_call_id.as_deref() == Some("provider-late")
            && message.content.contains("still unfinished")
    }));
    assert!(after_timeout
        .transcript
        .iter()
        .any(|message| message.content.contains("timed out")));
    pending
        .succeed(json!({
            "message": "late result completed",
            "value": "eventual",
        }))
        .unwrap();
    after_timeout
        .respond_text("I will incorporate the late result.")
        .unwrap();

    let after_result = world.request().await;
    assert!(after_result.transcript.iter().any(|message| {
        message
            .content
            .contains("A previously unfinished tool invocation has now returned")
            && message.content.contains("late result completed")
    }));
    after_result
        .respond_call("provider-end-after-late-result", "end", json!({}))
        .unwrap();
    world
        .wait_for_state(&session_id, |state| {
            state.last_turn_outcome == Some(TurnOutcome::Finished)
        })
        .await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
// Contract: docs/zork-agent-architecture.md [EVENT-05, PERSIST-02]
async fn every_external_effect_observes_its_durable_preparation_without_replaying_history() {
    let mut world = TestWorld::new();
    let mut echo = world
        .install_tool(controlled_tool("test.prepared"))
        .unwrap();
    let session_id = session(&world, "/virtual/effect-order").await;

    world
        .send_mail(&session_id, "run prepared work")
        .await
        .unwrap();
    let provider = world.request().await;
    let recoveries_before_effects = world.recovery_calls(&session_id);
    assert!(world.events(&session_id).iter().any(|envelope| matches!(
        &envelope.event,
        SessionEvent::StepStarted { step_id, .. } if step_id == &provider.step_id
    )));
    provider
        .respond_call(
            "provider-prepared",
            "test.prepared",
            json!({"value": "ready"}),
        )
        .unwrap();

    let tool = echo.request().await;
    assert_eq!(world.recovery_calls(&session_id), recoveries_before_effects);
    assert!(world.events(&session_id).iter().any(|envelope| matches!(
        &envelope.event,
        SessionEvent::StepCompleted { invocations, .. }
            if invocations.iter().any(|invocation| {
                invocation.invocation_id == tool.context.invocation_id
                    && invocation.tool == "test.prepared"
            })
    )));
    tool.succeed(json!({
        "message": "prepared work finished",
        "value": "ready",
    }))
    .unwrap();
    world
        .request()
        .await
        .respond_call("provider-prepared-end", "end", json!({}))
        .unwrap();
    world
        .wait_for_state(&session_id, |state| {
            state.last_turn_outcome == Some(TurnOutcome::Finished)
        })
        .await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
// Contract: docs/zork-agent-architecture.md [PERSIST-02, SUPERVISOR-01]
async fn an_idle_session_recovers_once_then_keeps_folding_only_new_events_while_active() {
    let mut world = TestWorld::new();
    let session_id = session(&world, "/virtual/idle-rebuild").await;

    world.send_mail(&session_id, "first turn").await.unwrap();
    world
        .request()
        .await
        .respond_call("provider-first-end", "end", json!({}))
        .unwrap();
    world
        .wait_for_state(&session_id, |state| {
            state.last_turn_outcome == Some(TurnOutcome::Finished)
        })
        .await;
    world
        .wait_for_slot(&session_id, PublicSlotStatus::Idle)
        .await;
    let recoveries_before_second_turn = world.recovery_calls(&session_id);

    world.send_mail(&session_id, "second turn").await.unwrap();
    let second = world.request().await;
    assert_eq!(
        world.recovery_calls(&session_id),
        recoveries_before_second_turn + 1
    );
    second.respond_text("one active step").unwrap();
    let third = world.request().await;
    assert_eq!(
        world.recovery_calls(&session_id),
        recoveries_before_second_turn + 1
    );
    third
        .respond_call("provider-second-end", "end", json!({}))
        .unwrap();
    world
        .wait_for_state(&session_id, |state| {
            state.last_turn_outcome == Some(TurnOutcome::Finished) && state.active_turn.is_none()
        })
        .await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
// Contract: docs/zork-agent-architecture.md [TOOL-07]
async fn a_tool_version_change_during_a_provider_request_returns_one_knowledge_updating_result() {
    let mut world = TestWorld::new();
    let _version_one = world
        .install_tool(controlled_tool("test.versioned"))
        .unwrap();
    let session_id = session(&world, "/virtual/tool-version-race").await;

    world
        .send_mail(&session_id, "call the versioned tool")
        .await
        .unwrap();
    let request = world.request().await;
    let mut replacement = controlled_tool("test.versioned");
    replacement.version = ToolVersion::new("test-2").unwrap();
    replacement.initial_description = "new initial contract".into();
    replacement.detailed_description = "new detailed contract".into();
    let _version_two = world.install_tool(replacement).unwrap();
    request
        .respond_call(
            "provider-versioned",
            "test.versioned",
            json!({"value": "race"}),
        )
        .unwrap();

    let after_change = tokio::time::timeout(Duration::from_secs(1), world.request())
        .await
        .expect("version mismatch returned a normal ToolResult without executing either instance");
    assert!(after_change
        .transcript
        .iter()
        .any(|message| { message.content.contains("Tool test.versioned has changed") }));
    assert!(!after_change
        .transcript
        .iter()
        .any(|message| message.content.contains("Tool test.versioned was updated")));
    assert_eq!(
        world.state(&session_id).await.unwrap().known_tools["test.versioned"].as_str(),
        "test-2"
    );
    after_change
        .respond_call("provider-version-end", "end", json!({}))
        .unwrap();
    world
        .wait_for_state(&session_id, |state| {
            state.last_turn_outcome == Some(TurnOutcome::Finished)
        })
        .await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
// Contract: docs/zork-agent-architecture.md [TOOL-05, TOOL-09]
async fn tool_change_notices_are_brief_once_per_session_and_readdition_is_new() {
    let mut world = TestWorld::new();
    let _version_one = world.install_tool(controlled_tool("test.catalog")).unwrap();
    let first_session = session(&world, "/virtual/catalog-one").await;
    let second_session = session(&world, "/virtual/catalog-two").await;

    world
        .send_mail(&first_session, "observe the catalog")
        .await
        .unwrap();
    let first = world.request().await;
    assert!(first
        .transcript
        .iter()
        .any(|message| message.content.contains("Run controlled test.catalog work")));

    let mut version_two = controlled_tool("test.catalog");
    version_two.version = ToolVersion::new("test-2").unwrap();
    version_two.initial_description = "updated initial summary".into();
    version_two.detailed_description = "DETAIL THAT MUST REQUIRE HELP".into();
    let _updated = world.install_tool(version_two).unwrap();
    first.respond_text("catalog observed").unwrap();

    let updated = world.request().await;
    assert_eq!(
        updated
            .transcript
            .iter()
            .filter(|message| message.content.contains("Tool test.catalog was updated"))
            .count(),
        1
    );
    assert!(!updated
        .transcript
        .iter()
        .any(|message| message.content.contains("DETAIL THAT MUST REQUIRE HELP")));
    assert_eq!(
        world.state(&first_session).await.unwrap().known_tools["test.catalog"].as_str(),
        "test-2"
    );
    assert_eq!(
        world.state(&second_session).await.unwrap().known_tools["test.catalog"].as_str(),
        "test-1"
    );

    updated.respond_text("continue").unwrap();
    let unchanged = world.request().await;
    assert_eq!(
        unchanged
            .transcript
            .iter()
            .filter(|message| message.content.contains("Tool test.catalog was updated"))
            .count(),
        1
    );

    assert!(world.tools.remove("test.catalog"));
    unchanged.respond_text("observe removal").unwrap();
    let removed = world.request().await;
    assert!(removed
        .transcript
        .iter()
        .any(|message| message.content.contains("Tool test.catalog was removed")));
    assert!(!world
        .state(&first_session)
        .await
        .unwrap()
        .known_tools
        .contains_key("test.catalog"));

    let mut version_three = controlled_tool("test.catalog");
    version_three.version = ToolVersion::new("test-3").unwrap();
    let _readded = world.install_tool(version_three).unwrap();
    removed.respond_text("observe readdition").unwrap();
    let added = world.request().await;
    assert!(added
        .transcript
        .iter()
        .any(|message| message.content.contains("Tool test.catalog was added")));
    added
        .respond_call("provider-catalog-end", "end", json!({}))
        .unwrap();
    world
        .wait_for_state(&first_session, |state| {
            state.last_turn_outcome == Some(TurnOutcome::Finished)
        })
        .await;

    world
        .send_mail(&second_session, "observe the shared registry")
        .await
        .unwrap();
    let second_request = world.request().await;
    assert!(second_request
        .transcript
        .iter()
        .any(|message| message.content.contains("Tool test.catalog was updated")));
    assert_eq!(
        world.state(&second_session).await.unwrap().known_tools["test.catalog"].as_str(),
        "test-3"
    );
    second_request
        .respond_call("provider-catalog-second-end", "end", json!({}))
        .unwrap();
    world
        .wait_for_state(&second_session, |state| {
            state.last_turn_outcome == Some(TurnOutcome::Finished)
        })
        .await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
// Contract: docs/zork-agent-architecture.md [TOOL-12]
async fn tool_cancel_is_durable_before_the_target_cancelled_result_and_never_replaces_it() {
    let mut options = ServiceOptions::default();
    options.runner.auto_wait = Duration::from_millis(10);
    let mut world = TestWorld::with_options(options);
    let mut slow = world
        .install_tool(controlled_tool("test.cancellable"))
        .unwrap();
    let session_id = session(&world, "/virtual/tool-cancel").await;

    world
        .send_mail(&session_id, "start cancellable work")
        .await
        .unwrap();
    world
        .request()
        .await
        .respond_call(
            "provider-cancellable",
            "test.cancellable",
            json!({"value": "slow"}),
        )
        .unwrap();
    let pending = slow.request().await;
    let target_id = pending.context.invocation_id.clone();
    world
        .wait_for_state(&session_id, |state| state.auto_wait.is_some())
        .await;
    wait_for_clock_timer(&world).await;
    world.clock.advance(Duration::from_millis(10));

    world
        .request()
        .await
        .respond_call(
            "provider-cancel-control",
            "tool.cancel",
            json!({"invocation_id": target_id}),
        )
        .unwrap();
    world
        .wait_for_state(&session_id, |state| {
            state.pending_tools.get(&target_id).is_some_and(|pending| {
                pending.cancel_requested
                    && pending
                        .result
                        .as_ref()
                        .is_some_and(|result| result.outcome == ToolOutcome::Cancelled)
            })
        })
        .await;

    let events = world.events(&session_id);
    let cancel_index = events
        .iter()
        .position(|event| matches!(
            &event.event,
            SessionEvent::ToolCancelRequested { invocation_id, .. } if invocation_id == &target_id
        ))
        .unwrap();
    let target_result_index = events
        .iter()
        .position(|event| {
            matches!(
                &event.event,
                SessionEvent::ToolResult { result }
                    if result.invocation_id == target_id && result.outcome == ToolOutcome::Cancelled
            )
        })
        .unwrap();
    assert!(cancel_index < target_result_index);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                &event.event,
                SessionEvent::ToolResult { result } if result.invocation_id == target_id
            ))
            .count(),
        1
    );

    world
        .request()
        .await
        .respond_call("provider-cancel-end", "end", json!({}))
        .unwrap();
    world
        .wait_for_state(&session_id, |state| {
            state.last_turn_outcome == Some(TurnOutcome::Finished)
        })
        .await;
    drop(pending);
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
// Contract: docs/zork-agent-architecture.md [TURN-02, PROJECTION-03]
async fn end_can_acknowledge_a_visible_outstanding_item_and_late_completion_remains_a_notice() {
    let mut options = ServiceOptions::default();
    options.runner.auto_wait = Duration::from_millis(10);
    let mut world = TestWorld::with_options(options);
    let mut slow = world
        .install_tool(controlled_tool("test.acknowledged"))
        .unwrap();
    let session_id = session(&world, "/virtual/acknowledge-outstanding").await;

    world
        .send_mail(&session_id, "start acknowledged work")
        .await
        .unwrap();
    world
        .request()
        .await
        .respond_call(
            "provider-acknowledged",
            "test.acknowledged",
            json!({"value": "eventual"}),
        )
        .unwrap();
    let pending = slow.request().await;
    world
        .wait_for_state(&session_id, |state| state.auto_wait.is_some())
        .await;
    wait_for_clock_timer(&world).await;
    world.clock.advance(Duration::from_millis(10));

    let outstanding = world.request().await;
    assert!(outstanding
        .transcript
        .iter()
        .any(|message| message.content.contains("still unfinished")));
    outstanding
        .respond_call(
            "provider-ack-end",
            "end",
            json!({"acknowledge_outstanding": true}),
        )
        .unwrap();
    world
        .wait_for_state(&session_id, |state| {
            state.last_turn_outcome == Some(TurnOutcome::Finished) && state.active_turn.is_none()
        })
        .await;
    assert!(world.events(&session_id).iter().any(|event| matches!(
        &event.event,
        SessionEvent::TurnFinished {
            outcome: TurnOutcome::Finished,
            outstanding,
            ..
        } if outstanding.iter().any(|item| item.id == pending.context.invocation_id)
    )));

    pending
        .succeed(json!({
            "message": "acknowledged work later completed",
            "value": "eventual",
        }))
        .unwrap();
    world
        .wait_for_state(&session_id, |state| {
            state.generation.entries.iter().any(|entry| {
                matches!(
                    entry,
                    GenerationEntry::ToolDelivery {
                        delivery: ToolDelivery::Result { result, .. },
                    } if result.data["message"] == "acknowledged work later completed"
                )
            })
        })
        .await;
    let next_turn = world.request().await;
    assert!(next_turn.transcript.iter().any(|message| {
        message
            .content
            .contains("A previously unfinished tool invocation has now returned")
            && message
                .content
                .contains("acknowledged work later completed")
    }));
    next_turn
        .respond_call("provider-after-ack-end", "end", json!({}))
        .unwrap();
    world
        .wait_for_state(&session_id, |state| {
            state.last_turn_outcome == Some(TurnOutcome::Finished) && state.active_turn.is_none()
        })
        .await;
    world.shutdown().await;
}

async fn wait_for_clock_timer(world: &TestWorld) {
    tokio::time::timeout(Duration::from_secs(5), world.clock.wait_for_pending_timer())
        .await
        .expect("zork-agent did not arm the expected virtual timer");
}

async fn wait_for_clock_deadline(world: &TestWorld, expected_deadline_ms: i64) {
    tokio::time::timeout(
        Duration::from_secs(5),
        world.clock.wait_for_pending_deadline(expected_deadline_ms),
    )
    .await
    .unwrap_or_else(|_| panic!("zork-agent did not arm virtual deadline {expected_deadline_ms}"));
}
