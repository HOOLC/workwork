use std::time::Duration;

use reqwest::{Client, Response, StatusCode};
use serde_json::json;
use zork_agent::session::events::{RuntimeFailure, SessionEvent, TurnOutcome};
use zork_agent::session::service::ServiceOptions;
use zork_agent::session::store::SessionStore;
use zork_agent::session::supervisor::{PublicSlotStatus, SupervisorError};
use zork_agent::session::wire::SessionSelection;
use zork_agent_api::{ApiErrorBody, ApiErrorCode, MailboxRequest};
use zork_agent_testkit::{AgentHttpServer, PendingModelRequest, TestWorld};

fn selection() -> SessionSelection {
    SessionSelection {
        profile_id: "test-profile".into(),
        model: "test-model".into(),
        thinking: "medium".into(),
    }
}

#[tokio::test(flavor = "multi_thread")]
// Contract: docs/zork-agent-architecture.md [SUPERVISOR-04]
async fn runtime_fault_is_visible_and_two_completed_steps_reset_its_streak() {
    let mut world = TestWorld::new();
    let session_id = world
        .create_session(selection(), None, "/virtual/runtime-fault-reset")
        .await
        .unwrap();
    world
        .send_mail(&session_id, "continue despite faults")
        .await
        .unwrap();

    request(&mut world, "first provider step")
        .await
        .panic_model_task("controlled model task panic")
        .unwrap();
    let recovered = request(&mut world, "request after first rebuild").await;
    assert!(recovered.transcript.iter().any(|message| {
        message.content.contains("Agent runtime failure")
            && message.content.contains("controlled model task panic")
            && message.content.contains("consecutive occurrence 1")
    }));
    recovered.respond_text("first healthy step").unwrap();

    request(&mut world, "second healthy step")
        .await
        .respond_text("second healthy step")
        .unwrap();
    request(&mut world, "fault after two healthy steps")
        .await
        .panic_model_task("controlled model task panic")
        .unwrap();

    let after_reset = request(&mut world, "request after reset fault").await;
    let counts = world
        .events(&session_id)
        .iter()
        .filter_map(|event| match &event.event {
            SessionEvent::RuntimeFault {
                consecutive_count, ..
            } => Some(*consecutive_count),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(counts, vec![1, 1]);
    assert!(after_reset.transcript.iter().any(|message| {
        message.content.contains("Agent runtime failure")
            && message.content.contains("consecutive occurrence 1")
    }));
    after_reset
        .respond_call("provider-end", "end", json!({}))
        .unwrap();
    world
        .wait_for_state(&session_id, |state| {
            state.last_turn_outcome == Some(TurnOutcome::Finished)
        })
        .await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
// Contract: docs/zork-agent-architecture.md [SUPERVISOR-04]
async fn a_different_runtime_fault_fingerprint_starts_a_new_streak() {
    let mut world = TestWorld::new();
    let session_id = world
        .create_session(selection(), None, "/virtual/runtime-fault-fingerprint")
        .await
        .unwrap();
    world
        .store
        .append_batch(
            &session_id,
            &[SessionEvent::RuntimeFault {
                failure: RuntimeFailure {
                    failure_id: "storage_task".into(),
                    stage: "store.append".into(),
                    message: "controlled storage failure".into(),
                },
                consecutive_count: 4,
                occurred_at_ms: world.clock.current_ms(),
            }],
        )
        .unwrap();
    world.restart().await.unwrap();

    let first = request(&mut world, "request after storage fault").await;
    assert!(first
        .transcript
        .iter()
        .any(|message| message.content.contains("controlled storage failure")));
    first
        .panic_model_task("different model task failure")
        .unwrap();
    let recovered = request(&mut world, "request after different model fault").await;
    let counts = world
        .events(&session_id)
        .iter()
        .filter_map(|event| match &event.event {
            SessionEvent::RuntimeFault {
                consecutive_count, ..
            } => Some(*consecutive_count),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(counts, vec![4, 1]);
    recovered
        .respond_call("provider-fingerprint-end", "end", json!({}))
        .unwrap();
    world
        .wait_for_state(&session_id, |state| {
            state.last_turn_outcome == Some(TurnOutcome::Finished)
        })
        .await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
// Contract: docs/zork-agent-architecture.md [SUPERVISOR-04]
async fn fifth_consecutive_runtime_fault_opens_only_that_sessions_circuit() {
    let mut options = ServiceOptions::default();
    options.supervisor.runner_fault_limit = 5;
    let mut world = TestWorld::with_options(options);
    let failing_session = world
        .create_session(selection(), None, "/virtual/runtime-fault-circuit")
        .await
        .unwrap();
    let healthy_session = world
        .create_session(selection(), None, "/virtual/runtime-fault-healthy")
        .await
        .unwrap();
    world
        .send_mail(&failing_session, "exercise the circuit")
        .await
        .unwrap();

    let mut pending = Some(request(&mut world, "fault occurrence 1").await);
    for occurrence in 1..=5 {
        pending
            .take()
            .expect("each non-final fault rebuilds one pending request")
            .panic_model_task("repeated controlled model task panic")
            .unwrap();
        if occurrence < 5 {
            let rebuilt = request(&mut world, &format!("rebuild after fault {occurrence}")).await;
            assert!(rebuilt.transcript.iter().any(|message| {
                message.content.contains("Agent runtime failure")
                    && message
                        .content
                        .contains(&format!("consecutive occurrence {occurrence}"))
            }));
            pending = Some(rebuilt);
        }
    }

    world
        .wait_for_slot(&failing_session, PublicSlotStatus::CircuitOpen)
        .await;
    let counts = world
        .events(&failing_session)
        .iter()
        .filter_map(|event| match &event.event {
            SessionEvent::RuntimeFault {
                consecutive_count, ..
            } => Some(*consecutive_count),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(counts, vec![1, 2, 3, 4, 5]);

    world
        .send_mail(&healthy_session, "the other session still runs")
        .await
        .unwrap();
    request(&mut world, "healthy session request")
        .await
        .respond_call("healthy-end", "end", json!({}))
        .unwrap();
    world
        .wait_for_state(&healthy_session, |state| {
            state.last_turn_outcome == Some(TurnOutcome::Finished)
        })
        .await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
// Contract: docs/zork-agent-architecture.md [SUPERVISOR-02]
async fn paused_durable_append_proves_session_and_global_admission_are_bounded() {
    let mut session_options = ServiceOptions::default();
    session_options.supervisor.session_queue_capacity = 1;
    session_options.supervisor.global_queue_capacity = 16;
    let mut session_world = TestWorld::with_options(session_options);
    let session_id = session_world
        .create_session(selection(), None, "/virtual/session-capacity")
        .await
        .unwrap();
    let pause = session_world.store.pause_appends();
    let service = session_world.service_handle();
    let first = spawn_input(service.clone(), session_id.clone(), "first");
    wait_for_blocked_append(&session_world).await;
    let second = spawn_input(service.clone(), session_id.clone(), "second");
    let third = spawn_input(service, session_id.clone(), "third");
    let (overloaded, queued) = first_completed(second, third).await;
    assert_eq!(overloaded, Err(SupervisorError::SessionOverloaded));
    drop(pause);
    assert_eq!(first.await.unwrap(), Ok(()));
    assert_eq!(queued.await.unwrap(), Ok(()));
    session_world.shutdown().await;

    let mut global_options = ServiceOptions::default();
    global_options.supervisor.session_queue_capacity = 8;
    global_options.supervisor.global_queue_capacity = 2;
    let mut global_world = TestWorld::with_options(global_options);
    let session_id = global_world
        .create_session(selection(), None, "/virtual/global-capacity")
        .await
        .unwrap();
    let pause = global_world.store.pause_appends();
    let service = global_world.service_handle();
    let first = spawn_input(service.clone(), session_id.clone(), "first");
    wait_for_blocked_append(&global_world).await;
    let second = spawn_input(service.clone(), session_id.clone(), "second");
    let third = spawn_input(service, session_id, "third");
    let (overloaded, queued) = first_completed(second, third).await;
    assert_eq!(overloaded, Err(SupervisorError::GlobalOverloaded));
    drop(pause);
    assert_eq!(first.await.unwrap(), Ok(()));
    assert_eq!(queued.await.unwrap(), Ok(()));
    global_world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
// Contract: docs/zork-agent-architecture.md [DELETE-01]
async fn delete_rejects_new_work_before_waiting_for_the_active_runner() {
    let mut world = TestWorld::new();
    let session_id = world
        .create_session(selection(), None, "/virtual/delete-race")
        .await
        .unwrap();
    let pause = world.store.pause_appends();
    let service = world.service_handle();
    let accepted = spawn_input(service.clone(), session_id.clone(), "already accepted");
    wait_for_blocked_append(&world).await;

    let deleting_service = service.clone();
    let deleting_session = session_id.clone();
    let deleting = tokio::spawn(async move { deleting_service.delete(&deleting_session).await });
    world
        .wait_for_slot(&session_id, PublicSlotStatus::Deleting)
        .await;
    assert_eq!(
        service.submit_input(&session_id, "too late".into()).await,
        Err(SupervisorError::Deleting)
    );

    drop(pause);
    assert_eq!(accepted.await.unwrap(), Ok(()));
    assert_eq!(deleting.await.unwrap(), Ok(()));
    assert!(!service.contains(&session_id));
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
// Contract: docs/zork-agent-architecture.md [SUPERVISOR-02]
async fn bounded_admission_is_reported_as_real_http_429_and_503() {
    let mut session_options = ServiceOptions::default();
    session_options.supervisor.session_queue_capacity = 1;
    session_options.supervisor.global_queue_capacity = 16;
    let mut session_world = TestWorld::with_options(session_options);
    let session_id = session_world
        .create_session(selection(), None, "/virtual/http-session-capacity")
        .await
        .unwrap();
    let server = AgentHttpServer::for_service(session_world.service_handle()).unwrap();
    let pause = session_world.store.pause_appends();
    let first = spawn_http_input(server.base_url(), &session_id, "first");
    wait_for_blocked_append(&session_world).await;
    let second = spawn_http_input(server.base_url(), &session_id, "second");
    let third = spawn_http_input(server.base_url(), &session_id, "third");
    let (overloaded, queued) = first_http_completed(second, third).await;
    assert_eq!(overloaded.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        overloaded.json::<ApiErrorBody>().await.unwrap().error.code,
        ApiErrorCode::SessionOverloaded
    );
    drop(pause);
    assert_eq!(first.await.unwrap().status(), StatusCode::ACCEPTED);
    assert_eq!(queued.await.unwrap().status(), StatusCode::ACCEPTED);
    server.shutdown().await;
    session_world.shutdown().await;

    let mut global_options = ServiceOptions::default();
    global_options.supervisor.session_queue_capacity = 8;
    global_options.supervisor.global_queue_capacity = 2;
    let mut global_world = TestWorld::with_options(global_options);
    let session_id = global_world
        .create_session(selection(), None, "/virtual/http-global-capacity")
        .await
        .unwrap();
    let server = AgentHttpServer::for_service(global_world.service_handle()).unwrap();
    let pause = global_world.store.pause_appends();
    let first = spawn_http_input(server.base_url(), &session_id, "first");
    wait_for_blocked_append(&global_world).await;
    let second = spawn_http_input(server.base_url(), &session_id, "second");
    let third = spawn_http_input(server.base_url(), &session_id, "third");
    let (overloaded, queued) = first_http_completed(second, third).await;
    assert_eq!(overloaded.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        overloaded.json::<ApiErrorBody>().await.unwrap().error.code,
        ApiErrorCode::GlobalOverloaded
    );
    drop(pause);
    assert_eq!(first.await.unwrap().status(), StatusCode::ACCEPTED);
    assert_eq!(queued.await.unwrap().status(), StatusCode::ACCEPTED);
    server.shutdown().await;
    global_world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
// Contract: docs/zork-agent-architecture.md [SUPERVISOR-03]
async fn an_accepted_command_is_not_replayed_after_its_runner_fails() {
    let mut options = ServiceOptions::default();
    options.supervisor.session_queue_capacity = 1;
    options.supervisor.global_queue_capacity = 16;
    let mut world = TestWorld::with_options(options);
    let session_id = world
        .create_session(selection(), None, "/virtual/no-dispatch-replay")
        .await
        .unwrap();
    let pause = world.store.pause_appends();
    let service = world.service_handle();
    let first = spawn_input(service.clone(), session_id.clone(), "first unconfirmed");
    wait_for_blocked_append(&world).await;
    let second = spawn_input(service.clone(), session_id.clone(), "second accepted");
    let third = spawn_input(service, session_id.clone(), "third competing");
    let (overloaded, accepted) = first_completed(second, third).await;
    assert_eq!(overloaded, Err(SupervisorError::SessionOverloaded));

    pause.release_with_error("controlled append failure");
    assert!(matches!(
        first.await.unwrap(),
        Err(SupervisorError::Runner(message)) if message.contains("controlled append failure")
    ));
    assert!(matches!(
        accepted.await.unwrap(),
        Err(SupervisorError::Runner(message)) if message.contains("before durable confirmation")
    ));

    world
        .wait_for_state(&session_id, |state| state.fault_streak.is_some())
        .await;
    world
        .send_mail(&session_id, "fresh after recovery")
        .await
        .unwrap();
    let inputs = world
        .events(&session_id)
        .into_iter()
        .filter_map(|envelope| match envelope.event {
            SessionEvent::InputAppended { input } => Some(input.content),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(inputs, ["fresh after recovery"]);
    assert!(world
        .events(&session_id)
        .iter()
        .any(|event| matches!(event.event, SessionEvent::RuntimeFault { .. })));
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
// Contract: docs/zork-agent-architecture.md [STARTUP-01, STARTUP-02]
async fn requested_unchecked_session_bypasses_background_scan_and_is_recovered_once() {
    let mut options = ServiceOptions::default();
    // One controlled background worker keeps `requested` unclaimed while the
    // first recovery is paused. Production's bounded workers have the same
    // contract for every slot they have not claimed yet.
    options.supervisor.startup_recovery_concurrency = 1;
    let mut world = TestWorld::with_options(options);
    let oldest = world
        .create_session(selection(), None, "/virtual/startup-oldest")
        .await
        .unwrap();
    let requested = world
        .create_session(selection(), None, "/virtual/startup-requested")
        .await
        .unwrap();
    let newest = world
        .create_session(selection(), None, "/virtual/startup-newest")
        .await
        .unwrap();
    let pause = world.query.pause_recovery(newest.clone());

    world.restart().await.unwrap();
    wait_for_blocked_recovery(&world, &newest).await;
    assert!(world.sessions().iter().any(|slot| {
        slot.session_id == requested && slot.status == PublicSlotStatus::Unchecked
    }));

    let state = world.state(&requested).await.unwrap();
    assert_eq!(state.session_id, requested);
    assert_eq!(world.recovery_calls(&requested), 1);

    drop(pause);
    tokio::time::timeout(Duration::from_secs(1), async {
        while world.recovery_calls(&newest) != 1 || world.recovery_calls(&oldest) != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("background discovery resumed and completed");
    assert_eq!(world.recovery_calls(&requested), 1);
    world.shutdown().await;
}

async fn request(world: &mut TestWorld, stage: &str) -> PendingModelRequest {
    tokio::time::timeout(Duration::from_secs(1), world.request())
        .await
        .unwrap_or_else(|_| panic!("zork-agent did not issue the expected request for {stage}"))
}

fn spawn_input(
    service: std::sync::Arc<zork_agent::session::service::SessionService>,
    session_id: String,
    content: &'static str,
) -> tokio::task::JoinHandle<Result<(), SupervisorError>> {
    tokio::spawn(async move { service.submit_input(&session_id, content.to_owned()).await })
}

async fn first_completed(
    mut left: tokio::task::JoinHandle<Result<(), SupervisorError>>,
    mut right: tokio::task::JoinHandle<Result<(), SupervisorError>>,
) -> (
    Result<(), SupervisorError>,
    tokio::task::JoinHandle<Result<(), SupervisorError>>,
) {
    tokio::time::timeout(Duration::from_secs(1), async {
        tokio::select! {
            result = &mut left => (result.unwrap(), right),
            result = &mut right => (result.unwrap(), left),
        }
    })
    .await
    .expect("one excess external submission is rejected immediately")
}

fn spawn_http_input(
    base_url: &str,
    session_id: &str,
    content: &'static str,
) -> tokio::task::JoinHandle<Response> {
    let client = Client::new();
    let url = format!("{base_url}/sessions/{session_id}/mailbox");
    tokio::spawn(async move {
        client
            .post(url)
            .json(&MailboxRequest {
                content: content.to_owned(),
            })
            .send()
            .await
            .unwrap()
    })
}

async fn first_http_completed(
    mut left: tokio::task::JoinHandle<Response>,
    mut right: tokio::task::JoinHandle<Response>,
) -> (Response, tokio::task::JoinHandle<Response>) {
    tokio::time::timeout(Duration::from_secs(1), async {
        tokio::select! {
            result = &mut left => (result.unwrap(), right),
            result = &mut right => (result.unwrap(), left),
        }
    })
    .await
    .expect("one excess HTTP submission is rejected immediately")
}

async fn wait_for_blocked_append(world: &TestWorld) {
    tokio::time::timeout(Duration::from_secs(1), async {
        while world.store.blocked_appends() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the first input reached the paused durable append");
}

async fn wait_for_blocked_recovery(world: &TestWorld, session_id: &str) {
    tokio::time::timeout(Duration::from_secs(1), async {
        while world.query.blocked_recoveries(session_id) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("background discovery reached the paused session recovery");
}
