use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use serde_json::{json, Value};
use zork_agent::session::events::{
    OutstandingItem, SessionEvent, ToolOutcome, ToolResultData, TurnOutcome,
};
use zork_agent::session::state::{migrate_snapshot, snapshot_value, STATE_SCHEMA_VERSION};
use zork_agent::session::store::SessionStore;
use zork_agent::session::tools::{ToolCompatibility, ToolContract, ToolState, ToolVersion};
use zork_agent::session::wire::SessionSelection;
use zork_agent_testkit::TestWorld;

#[derive(Default)]
struct CounterCompatibility {
    result_migrations: AtomicUsize,
    state_migrations: AtomicUsize,
}

impl ToolCompatibility for CounterCompatibility {
    fn migrate_result(&self, schema_version: u32, value: Value) -> Result<Value, String> {
        if schema_version != 1 {
            return Err(format!("unsupported counter result {schema_version}"));
        }
        self.result_migrations.fetch_add(1, Ordering::Relaxed);
        Ok(json!({"delta": value["amount"]}))
    }

    fn migrate_state(&self, state: ToolState) -> Result<ToolState, String> {
        self.state_migrations.fetch_add(1, Ordering::Relaxed);
        match state.schema_version {
            1 => Ok(ToolState {
                schema_version: 2,
                value: json!({"total": state.value["total"]}),
            }),
            2 => Ok(state),
            other => Err(format!("unsupported counter state {other}")),
        }
    }

    fn initial_state(&self) -> Option<ToolState> {
        Some(ToolState {
            schema_version: 1,
            value: json!({"total": 0}),
        })
    }

    fn fold(&self, state: Option<&ToolState>, result: &Value) -> Result<Option<ToolState>, String> {
        let total = state
            .and_then(|state| state.value["total"].as_i64())
            .unwrap_or_default()
            + result["delta"].as_i64().unwrap_or_default();
        Ok(Some(ToolState {
            schema_version: 1,
            value: json!({"total": total}),
        }))
    }

    fn outstanding(&self, state: Option<&ToolState>) -> Vec<OutstandingItem> {
        let total = state
            .and_then(|state| state.value["total"].as_i64())
            .unwrap_or_default();
        if total >= 3 {
            Vec::new()
        } else {
            vec![OutstandingItem {
                kind: "counter".into(),
                id: "counter-goal".into(),
                summary: format!("counter needs {} more unit(s)", 3 - total),
            }]
        }
    }
}

fn selection() -> SessionSelection {
    SessionSelection {
        profile_id: "test-profile".into(),
        model: "test-model".into(),
        thinking: "medium".into(),
    }
}

fn counter_contract() -> ToolContract {
    ToolContract {
        name: "test.counter".into(),
        version: ToolVersion::new("agent-contract-blue").unwrap(),
        initial_description: "Increment the durable counter when requested.".into(),
        detailed_description: "Increment the durable counter with amount.".into(),
        input_schema: json!({
            "type": "object",
            "properties": {"amount": {"type": "integer"}},
            "required": ["amount"],
            "additionalProperties": false
        }),
    }
}

#[tokio::test(flavor = "multi_thread")]
// Contract: docs/zork-agent-architecture.md [TOOL-03, TOOL-04, TURN-01]
async fn tool_owned_migrations_fold_outstanding_snapshot_and_restart_share_one_state_contract() {
    let mut world = TestWorld::new();
    let compatibility = Arc::new(CounterCompatibility::default());
    let mut counter = world
        .install_stateful_tool(counter_contract(), compatibility.clone())
        .unwrap();
    let session_id = world
        .create_session(selection(), None, "/virtual/stateful-tool")
        .await
        .unwrap();

    world
        .send_mail(&session_id, "advance the counter")
        .await
        .unwrap();
    let first = world.request().await;
    assert!(first.transcript.iter().any(|message| {
        message
            .content
            .contains("Increment the durable counter when requested")
    }));
    first
        .respond_call("provider-counter-1", "test.counter", json!({"amount": 2}))
        .unwrap();
    counter
        .request()
        .await
        .succeed(json!({"message": "counter advanced", "amount": 2}))
        .unwrap();

    let second = world.request().await;
    let after_first = world.state(&session_id).await.unwrap();
    assert_eq!(after_first.tool_states["test.counter"].value["total"], 2);
    second.respond_text("the counter still needs work").unwrap();

    let after_text = world.request().await;
    assert!(after_text
        .transcript
        .iter()
        .any(|message| message.content.contains("counter needs 1 more unit")));
    after_text
        .respond_call("provider-counter-2", "test.counter", json!({"amount": 1}))
        .unwrap();
    counter
        .request()
        .await
        .succeed(json!({"message": "counter completed", "amount": 1}))
        .unwrap();

    world
        .request()
        .await
        .respond_call("provider-end", "end", json!({}))
        .unwrap();
    let finished = world
        .wait_for_state(&session_id, |state| {
            state.last_turn_outcome == Some(TurnOutcome::Finished)
        })
        .await;
    assert_eq!(finished.tool_states["test.counter"].value["total"], 3);
    assert!(finished.outstanding(&world.tools).is_empty());

    let snapshot = snapshot_value(&finished).unwrap();
    let migrated = migrate_snapshot(STATE_SCHEMA_VERSION, snapshot, &world.tools).unwrap();
    assert_eq!(migrated.tool_states["test.counter"].schema_version, 2);
    assert_eq!(migrated.tool_states["test.counter"].value["total"], 3);
    assert!(compatibility.result_migrations.load(Ordering::Relaxed) >= 2);
    assert!(compatibility.state_migrations.load(Ordering::Relaxed) >= 2);

    world.restart().await.unwrap();
    let restarted = world.state(&session_id).await.unwrap();
    assert_eq!(restarted.tool_states["test.counter"].value["total"], 3);
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
// Contract: docs/zork-agent-architecture.md [EVENT-06, RECOVERY-01]
async fn an_unmatched_durable_tool_result_is_folded_and_reported_as_new_information() {
    let mut world = TestWorld::new();
    let compatibility = Arc::new(CounterCompatibility::default());
    world
        .install_stateful_tool(counter_contract(), compatibility)
        .unwrap();
    let session_id = world
        .create_session(selection(), None, "/virtual/unmatched-result")
        .await
        .unwrap();

    world.send_mail(&session_id, "finish first").await.unwrap();
    world
        .request()
        .await
        .respond_call("provider-end-first", "end", json!({}))
        .unwrap();
    world
        .wait_for_state(&session_id, |state| {
            state.last_turn_outcome == Some(TurnOutcome::Finished)
        })
        .await;

    world
        .store
        .append_batch(
            &session_id,
            &[SessionEvent::ToolResult {
                result: ToolResultData {
                    invocation_id: "01ARZ3NDEKTSV4RRFFQ69G5FA2".into(),
                    tool: "test.counter".into(),
                    outcome: ToolOutcome::Succeeded,
                    data: json!({
                        "amount": 2,
                        "message": "late external completion",
                    }),
                    result_schema_version: 1,
                    knowledge: None,
                    finished_at_ms: 10,
                },
            }],
        )
        .unwrap();
    world.restart().await.unwrap();

    world
        .send_mail(&session_id, "continue after the late result")
        .await
        .unwrap();
    let request = world.request().await;
    assert!(request.transcript.iter().any(|message| {
        message
            .content
            .contains("did not match a currently pending invocation")
            && message.content.contains("late external completion")
    }));
    assert_eq!(
        world.state(&session_id).await.unwrap().tool_states["test.counter"].value["total"],
        2
    );
    request
        .respond_call(
            "provider-end-second",
            "end",
            json!({"acknowledge_outstanding": true}),
        )
        .unwrap();
    world
        .wait_for_state(&session_id, |state| {
            state.last_turn_outcome == Some(TurnOutcome::Finished) && state.active_turn.is_none()
        })
        .await;
    world.shutdown().await;
}
