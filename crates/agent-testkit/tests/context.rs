use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use zork_agent::session::events::{Purpose, SessionEvent, TurnOutcome};
use zork_agent::session::model::{ModelError, ModelOutcome, ModelTokenUsage, ProviderFailure};
use zork_agent::session::service::ServiceOptions;
use zork_agent::session::tools::{ToolContract, ToolVersion};
use zork_agent::session::wire::{SessionSelection, TranscriptRole};
use zork_agent_api::{ContextConfig, ContextStrategy};
use zork_agent_testkit::model::PendingModelRequest;
use zork_agent_testkit::TestWorld;

fn selection() -> SessionSelection {
    SessionSelection {
        profile_id: "context-test".into(),
        model: "same-model".into(),
        thinking: "max".into(),
    }
}

fn options(strategy: ContextStrategy) -> ServiceOptions {
    let mut options = ServiceOptions::default();
    options.runner.context = ContextConfig {
        strategy,
        keep_recent_tokens: 64,
    };
    options.runner.input_budget = Arc::new(|_| Some(10_000));
    options.runner.max_output_tokens = Arc::new(|_| Some(16_000));
    options.runner.context_attempt_limit = 2;
    options.runner.provider_retry_limit = 1;
    options
}

fn outcome(text: impl Into<String>, input_tokens: u64) -> ModelOutcome {
    ModelOutcome {
        text: text.into(),
        tool_calls: Vec::new(),
        provider_context: None,
        usage: Some(ModelTokenUsage {
            input_tokens,
            output_tokens: 17,
            cached_input_tokens: Some(0),
            output_reasoning_tokens: Some(0),
            output_text_tokens: Some(17),
        }),
        provider_input: None,
    }
}

async fn request(world: &mut TestWorld) -> PendingModelRequest {
    tokio::time::timeout(Duration::from_secs(3), world.request())
        .await
        .expect("next step")
}

async fn begin(world: &mut TestWorld) -> (String, PendingModelRequest) {
    let session = world
        .create_session(
            selection(),
            Some("Keep the task's constraints.".into()),
            "/virtual/context",
        )
        .await
        .unwrap();
    world
        .send_mail(
            &session,
            "Complete the original task; do not restart completed work.",
        )
        .await
        .unwrap();
    request(world)
        .await
        .respond(Ok(outcome("old progress ".repeat(1_000), 10_000)))
        .unwrap();
    let maintenance = request(world).await;
    (session, maintenance)
}

async fn finish(world: &mut TestWorld, session: &str, next: PendingModelRequest) {
    next.respond_call("end", "end", json!({})).unwrap();
    world
        .wait_for_state(session, |state| {
            state.last_turn_outcome == Some(TurnOutcome::Finished)
        })
        .await;
}

#[tokio::test(flavor = "multi_thread")]
// Contract: docs/zork-agent-architecture.md [CONTEXT-01, COMPACTION-01, PROVIDER-02]
async fn default_compaction_uses_an_independent_summary_and_retains_recent_context() {
    assert_eq!(
        ServiceOptions::default().runner.context.strategy,
        ContextStrategy::Compaction
    );
    let budget = Arc::new(AtomicU64::new(u64::MAX));
    let mut config = options(ContextStrategy::Compaction);
    let observed = budget.clone();
    config.runner.input_budget = Arc::new(move |_| Some(observed.load(Ordering::SeqCst)));
    let mut world = TestWorld::with_options(config);
    let session = world
        .create_session(selection(), None, "/virtual/tail")
        .await
        .unwrap();
    world.send_mail(&session, "original goal").await.unwrap();
    request(&mut world)
        .await
        .respond(Ok(outcome("old-material ".repeat(4_000), 500)))
        .unwrap();
    let next = request(&mut world).await;
    budget.store(10_000, Ordering::SeqCst);
    next.respond(Ok(outcome("recent fact: file.rs is ready", 10_000)))
        .unwrap();
    let summary = request(&mut world).await;
    assert!(summary.independent);
    assert!(summary.tools.is_empty());
    assert_eq!(summary.selection, selection());
    assert_eq!(summary.max_output_tokens, Some(16_000));
    assert!(summary
        .transcript
        .iter()
        .any(|message| message.content.contains("original goal")));
    assert!(summary
        .transcript
        .iter()
        .all(|message| message.tool_calls.is_empty() && message.provider_context.is_none()));
    let before = world.state(&session).await.unwrap();
    let turn_id = before.active_turn.as_ref().unwrap().turn_id.clone();
    let anchor = before.token_anchor.clone();
    // This stream and empty result must not leak into user-facing output or
    // become the main conversation's usage anchor.
    let mut live = world.subscribe(&session).unwrap();
    summary.stream_text("internal summary delta");
    let mut invalid = outcome("", 7_777);
    invalid
        .tool_calls
        .push(zork_agent::session::wire::ProviderToolCall {
            tool_call_id: "must-not-end".into(),
            tool_name: "call".into(),
            arguments: json!({"tool": "end", "arguments": {}}),
        });
    summary.respond(Ok(invalid)).unwrap();
    let retry = request(&mut world).await;
    assert_eq!(world.state(&session).await.unwrap().token_anchor, anchor);
    assert!(retry.independent && retry.tools.is_empty());
    world
        .send_mail(&session, "new instruction during summary")
        .await
        .unwrap();
    retry
        .respond(Ok(outcome(
            "Summary: original goal; old work completed.",
            6_666,
        )))
        .unwrap();
    let successor = request(&mut world).await;
    assert!(!successor.independent);
    assert_eq!(successor.generation, 2);
    assert_eq!(successor.tools.len(), 1);
    let text = successor
        .transcript
        .iter()
        .map(|message| message.content.as_ref())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("Summary: original goal; old work completed."));
    assert!(text.contains("recent fact: file.rs is ready"));
    assert!(!text.contains("old-material"));
    assert_eq!(text.matches("new instruction during summary").count(), 1);
    let state = world.state(&session).await.unwrap();
    assert_eq!(state.active_turn.as_ref().unwrap().turn_id, turn_id);
    assert!(state.token_anchor.is_none());
    while let Ok(Ok(event)) = tokio::time::timeout(Duration::from_millis(1), live.recv()).await {
        assert!(!matches!(
            event,
            zork_agent::session::service::LiveSessionEvent::TextDelta { .. }
        ));
    }
    let summary_usage = world
        .events(&session)
        .iter()
        .filter_map(|envelope| match &envelope.event {
            SessionEvent::StepCompleted {
                purpose: Purpose::Compaction,
                usage: Some(usage),
                ..
            } => Some(usage.input_tokens),
            _ => None,
        })
        .sum::<u64>();
    assert_eq!(summary_usage, 7_777 + 6_666);
    finish(&mut world, &session, successor).await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
// Contract: docs/zork-agent-architecture.md [CONTEXT-01, HANDOFF-01, COMPACTION-01]
async fn strategy_changes_apply_to_the_next_operation_in_both_directions() {
    let mut world = TestWorld::with_options(options(ContextStrategy::Compaction));
    let (session, summary) = begin(&mut world).await;
    world
        .service_handle()
        .set_context(
            &session,
            ContextConfig {
                strategy: ContextStrategy::Handoff,
                keep_recent_tokens: 0,
            },
        )
        .await
        .unwrap();
    summary.respond_text("").unwrap();
    let summary_retry = request(&mut world).await;
    assert!(summary_retry.independent && summary_retry.tools.is_empty());
    summary_retry.respond_text("First summary.").unwrap();
    request(&mut world)
        .await
        .respond(Ok(outcome("more work", 10_000)))
        .unwrap();
    let handoff = request(&mut world).await;
    assert!(!handoff.independent && handoff.tools.len() == 1);
    assert!(handoff
        .transcript
        .iter()
        .any(|message| message.role == TranscriptRole::User
            && message.content.contains("A context handoff is required")));
    world
        .service_handle()
        .set_context(&session, ContextConfig::default())
        .await
        .unwrap();
    handoff
        .respond_call(
            "handoff",
            "handoff",
            json!({"document": "Handoff document.", "extra": "ignored"}),
        )
        .unwrap();
    request(&mut world)
        .await
        .respond(Ok(outcome("further work", 10_000)))
        .unwrap();
    let summary = request(&mut world).await;
    assert!(summary.independent);
    assert!(summary
        .transcript
        .iter()
        .any(|message| message.content.contains("Handoff document.")));
    summary.respond_text("Updated summary.").unwrap();
    let successor = request(&mut world).await;
    assert_eq!(successor.generation, 4);
    finish(&mut world, &session, successor).await;
    let applied = world
        .events(&session)
        .iter()
        .filter_map(|event| match event.event {
            SessionEvent::ContextApplied { purpose, .. } => Some(purpose),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        applied,
        vec![Purpose::Compaction, Purpose::Handoff, Purpose::Compaction]
    );
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
// Contract: docs/zork-agent-architecture.md [CONTEXT-02, HANDOFF-02, HANDOFF-03]
async fn both_modes_fall_back_without_losing_mail_or_late_tool_results() {
    for strategy in [ContextStrategy::Compaction, ContextStrategy::Handoff] {
        let mut world = TestWorld::with_options(options(strategy));
        let mut slow = world
            .install_tool(ToolContract {
                name: "test.slow".into(),
                version: ToolVersion::new("v1").unwrap(),
                initial_description: "A controlled slow operation".into(),
                detailed_description: "Wait for its real result".into(),
                input_schema: json!({"type": "object", "properties": {}}),
            })
            .unwrap();
        let session = world
            .create_session(selection(), None, "/virtual/late-result")
            .await
            .unwrap();
        world.send_mail(&session, "original task").await.unwrap();
        let first = request(&mut world).await;
        let mut response = outcome("working", 10_000);
        response
            .tool_calls
            .push(zork_agent::session::wire::ProviderToolCall {
                tool_call_id: "slow".into(),
                tool_name: "call".into(),
                arguments: json!({"tool": "test.slow", "arguments": {}}),
            });
        first.respond(Ok(response)).unwrap();
        let pending = slow.request().await;
        world
            .wait_for_state(&session, |state| state.auto_wait.is_some())
            .await;
        world
            .send_mail(&session, "arrived before maintenance")
            .await
            .unwrap();
        let attempt = request(&mut world).await;
        let turn_id = world
            .state(&session)
            .await
            .unwrap()
            .active_turn
            .unwrap()
            .turn_id;
        let invocation_id = pending.context.invocation_id.clone();
        pending
            .succeed(json!({"value": "authoritative late result"}))
            .unwrap();
        world
            .wait_for_state(&session, |state| {
                state
                    .pending_tools
                    .get(&invocation_id)
                    .is_some_and(|pending| pending.result.is_some())
            })
            .await;
        world
            .send_mail(&session, "arrived during maintenance")
            .await
            .unwrap();
        attempt.respond_text("").unwrap();
        request(&mut world).await.respond_text("").unwrap();
        let successor = request(&mut world).await;
        assert_eq!(successor.generation, 2);
        let text = successor
            .transcript
            .iter()
            .map(|message| message.content.as_ref())
            .collect::<Vec<_>>()
            .join("\n");
        for expected in [
            "authoritative late result",
            "arrived before maintenance",
            "arrived during maintenance",
        ] {
            assert_eq!(text.matches(expected).count(), 1, "{strategy:?}: {text}");
        }
        assert!(text.contains("history.list") && text.contains("SAME turn"));
        let state = world.state(&session).await.unwrap();
        assert_eq!(state.active_turn.as_ref().unwrap().turn_id, turn_id);
        assert!(state.generation.document.is_none());
        assert_eq!(
            world
                .events(&session)
                .iter()
                .filter(|event| matches!(&event.event,
            SessionEvent::ToolResult { result } if result.invocation_id == invocation_id))
                .count(),
            1
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(20), slow.request())
                .await
                .is_err()
        );
        finish(&mut world, &session, successor).await;
        world.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
// Contract: docs/zork-agent-architecture.md [CONTEXT-02, RETRY-01, PROVIDER-02]
async fn output_limit_exhaustion_is_document_failure_but_network_failure_is_not() {
    for strategy in [ContextStrategy::Compaction, ContextStrategy::Handoff] {
        let mut world = TestWorld::with_options(options(strategy));
        let (session, first) = begin(&mut world).await;
        let mut failure =
            ProviderFailure::new("provider.outcome.finish_reason", true, "incomplete output");
        failure.provider_code = Some("max_output_tokens".into());
        failure.request_id = Some("kept-request-id".into());
        failure.usage = Some(Box::new(outcome("", 222).usage.unwrap()));
        first
            .respond(Err(ModelError::ProviderFailed(failure.clone())))
            .unwrap();
        request(&mut world)
            .await
            .respond(Err(ModelError::ProviderFailed(failure)))
            .unwrap();
        let successor = request(&mut world).await;
        assert_eq!(successor.generation, 2);
        assert_eq!(world.events(&session).iter().filter(|event| matches!(&event.event,
            SessionEvent::StepFailed { error, .. } if error.request_id.as_deref() == Some("kept-request-id")
                && error.usage.as_ref().is_some_and(|usage| usage.input_tokens == 222))).count(), 2);
        finish(&mut world, &session, successor).await;
        world.shutdown().await;

        let mut world = TestWorld::with_options(options(strategy));
        let (session, attempt) = begin(&mut world).await;
        attempt
            .fail_provider("transport", true, "connection reset")
            .unwrap();
        let state = world
            .wait_for_state(&session, |state| {
                state.last_turn_outcome == Some(TurnOutcome::Failed)
            })
            .await;
        assert_eq!(state.generation.number, 1);
        assert!(!world
            .events(&session)
            .iter()
            .any(|event| matches!(event.event, SessionEvent::ContextApplied { .. })));
        world.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
// Contract: docs/zork-agent-architecture.md [CONTEXT-02, COMPACTION-01]
async fn an_overflow_first_tries_compaction_then_can_take_the_shared_empty_exit() {
    let mut world = TestWorld::with_options(options(ContextStrategy::Compaction));
    let session = world
        .create_session(selection(), None, "/virtual/overflow")
        .await
        .unwrap();
    world
        .send_mail(&session, "input rejected with the old context")
        .await
        .unwrap();
    let mut overflow = ProviderFailure::new("provider", false, "context too large");
    overflow.provider_code = Some("context_length_exceeded".into());
    request(&mut world)
        .await
        .respond(Err(ModelError::ProviderFailed(overflow.clone())))
        .unwrap();
    let summary = request(&mut world).await;
    assert!(summary.independent && summary.generation == 1);
    summary
        .respond(Err(ModelError::ProviderFailed(overflow)))
        .unwrap();
    let successor = request(&mut world).await;
    assert_eq!(successor.generation, 2);
    assert_eq!(
        successor
            .transcript
            .iter()
            .filter(|message| message.content.as_ref() == "input rejected with the old context")
            .count(),
        1
    );
    finish(&mut world, &session, successor).await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
// Contract: docs/zork-agent-architecture.md [CONTEXT-01, SNAPSHOT-01, RECOVERY-01]
async fn compaction_plan_survives_restart_and_does_not_change_with_configuration() {
    let mut world = TestWorld::with_options(options(ContextStrategy::Compaction));
    let (session, interrupted) = begin(&mut world).await;
    let progress = world
        .state(&session)
        .await
        .unwrap()
        .context_progress()
        .unwrap()
        .clone();
    world
        .service_handle()
        .set_context(
            &session,
            ContextConfig {
                strategy: ContextStrategy::Handoff,
                keep_recent_tokens: 0,
            },
        )
        .await
        .unwrap();
    world.send_mail(&session, "survive restart").await.unwrap();
    world.restart().await.unwrap();
    drop(interrupted);
    let recovered = request(&mut world).await;
    assert!(recovered.independent);
    assert_eq!(
        world.state(&session).await.unwrap().context_progress(),
        Some(&progress)
    );
    recovered.respond_text("Recovered summary").unwrap();
    let successor = request(&mut world).await;
    let transcript = successor.transcript.clone();
    world.restart().await.unwrap();
    drop(successor);
    let recovered = request(&mut world).await;
    assert!(!recovered.independent);
    assert!(recovered.transcript.starts_with(&transcript));
    assert_eq!(
        world.state(&session).await.unwrap().context_config.strategy,
        ContextStrategy::Handoff
    );
    finish(&mut world, &session, recovered).await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
// Contract: docs/zork-agent-architecture.md [COMPACTION-01, HANDOFF-02, PROJECTION-01]
async fn a_result_in_the_retained_original_tail_is_not_delivered_twice() {
    let mut config = options(ContextStrategy::Compaction);
    config.runner.context.keep_recent_tokens = 1_000;
    let mut world = TestWorld::with_options(config);
    let mut slow = world
        .install_tool(ToolContract {
            name: "test.slow".into(),
            version: ToolVersion::new("v1").unwrap(),
            initial_description: "Controlled operation".into(),
            detailed_description: "Wait for the result".into(),
            input_schema: json!({"type": "object", "properties": {}}),
        })
        .unwrap();
    let session = world
        .create_session(selection(), None, "/virtual/retained-result")
        .await
        .unwrap();
    world.send_mail(&session, "original task").await.unwrap();
    let mut response = outcome("working", 10_000);
    response
        .tool_calls
        .push(zork_agent::session::wire::ProviderToolCall {
            tool_call_id: "result-call".into(),
            tool_name: "call".into(),
            arguments: json!({"tool": "test.slow", "arguments": {}}),
        });
    request(&mut world).await.respond(Ok(response)).unwrap();
    slow.request()
        .await
        .succeed(json!({"body": "unique-real-result"}))
        .unwrap();
    let summary = request(&mut world).await;
    assert!(summary.independent);
    summary
        .respond_text("Task context before the retained tool call.")
        .unwrap();
    let successor = request(&mut world).await;
    assert_eq!(
        successor
            .transcript
            .iter()
            .filter(|message| message.content.contains("unique-real-result"))
            .count(),
        1
    );
    assert!(successor
        .transcript
        .iter()
        .any(|message| message.role == TranscriptRole::Tool
            && message.tool_call_id.as_deref() == Some("result-call")));
    assert!(world
        .state(&session)
        .await
        .unwrap()
        .pending_tools
        .is_empty());
    finish(&mut world, &session, successor).await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
// Contract: docs/zork-agent-architecture.md [CONTEXT-01, CANCEL-01]
async fn cancel_interrupts_either_context_step_without_committing_a_generation() {
    for strategy in [ContextStrategy::Compaction, ContextStrategy::Handoff] {
        let mut world = TestWorld::with_options(options(strategy));
        let (session, pending) = begin(&mut world).await;
        world.cancel(&session).await.unwrap();
        let state = world
            .wait_for_state(&session, |state| {
                state.last_turn_outcome == Some(TurnOutcome::Cancelled)
            })
            .await;
        assert_eq!(state.generation.number, 1);
        assert!(pending.respond_text("too late").is_err());
        assert!(!world
            .events(&session)
            .iter()
            .any(|event| matches!(event.event, SessionEvent::ContextApplied { .. })));
        world.shutdown().await;
    }
}
