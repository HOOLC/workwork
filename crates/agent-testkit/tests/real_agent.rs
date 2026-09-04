use serde_json::json;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use ulid::Ulid;
use zork_agent::session::event_id::EventId;
use zork_agent::session::events::{Input, SessionEvent, TurnOutcome, EVENT_SCHEMA_VERSION};
use zork_agent::session::store::EventEnvelope;
use zork_agent::session::wire::SessionSelection;
use zork_agent_testkit::{PendingHttpRequest, RealAgent};

fn body_contains(request: &PendingHttpRequest, needle: &str) -> bool {
    request
        .json()
        .expect("provider request is JSON")
        .to_string()
        .contains(needle)
}

async fn wait_for_file(path: &Path) -> String {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(content) = std::fs::read_to_string(path) {
                return content;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("file appeared before the test deadline")
}

fn active_segment(data_root: &Path, session_id: &str) -> PathBuf {
    let segments = data_root.join("sessions").join(session_id).join("segments");
    let mut active = std::fs::read_dir(segments)
        .expect("session segment directory")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "jsonl")
        })
        .collect::<Vec<_>>();
    active.sort();
    active.pop().expect("one active JSONL segment")
}

#[tokio::test(flavor = "multi_thread")]
// Contract: docs/zork-agent-architecture.md [ENV-01, TOOL-13, CANCEL-01, RECOVERY-01, DELETE-01, PERF-02]
async fn real_agent_uses_real_files_shell_store_and_recovers_after_restart() {
    let started = Instant::now();
    let mut agent = RealAgent::new().unwrap();
    let session_id = agent
        .create_session(
            SessionSelection {
                profile_id: "test-profile".into(),
                model: "test-model".into(),
                thinking: "medium".into(),
            },
            Some("real environment test".into()),
        )
        .await
        .unwrap();

    agent
        .send_mail(&session_id, "create and inspect note.txt")
        .await
        .unwrap();
    let first_request = agent.request().await;
    assert_eq!(first_request.path_and_query, "/v1/chat/completions");
    let first_body = first_request.json().expect("provider request body");
    assert_eq!(
        first_body["tools"]
            .as_array()
            .expect("provider tools")
            .iter()
            .filter_map(|tool| tool["function"]["name"].as_str())
            .collect::<Vec<_>>(),
        vec!["call"]
    );
    assert!(first_body["messages"]
        .as_array()
        .expect("provider messages")
        .iter()
        .any(|message| message["role"] == "user"
            && message["content"] == "create and inspect note.txt"));
    first_request
        .respond_openai_calls(
            "chatcmpl-1",
            [(
                "provider-call-1",
                "file.write",
                json!({"path": "note.txt", "content": "alpha"}),
            )],
        )
        .unwrap();

    let after_write = agent.request().await;
    assert_eq!(
        std::fs::read_to_string(agent.workspace(&session_id).unwrap().join("note.txt")).unwrap(),
        "alpha"
    );
    let write_result: serde_json::Value = serde_json::from_str(
        after_write.json().unwrap()["messages"]
            .as_array()
            .unwrap()
            .iter()
            .rev()
            .find(|message| message["role"] == "tool")
            .and_then(|message| message["content"].as_str())
            .expect("file.write returned one direct tool result"),
    )
    .unwrap();
    assert_eq!(write_result["data"]["bytes"], 5);
    assert!(write_result.get("message").is_none());
    after_write
        .respond_openai_calls(
            "chatcmpl-2",
            [(
                "provider-call-2",
                "shell.run",
                json!({
                    "command": "printf 'started\\n'; printf ' beta' >> note.txt; printf 'finished\\n'"
                }),
            )],
        )
        .unwrap();

    let after_shell = agent.request().await;
    assert_eq!(
        std::fs::read_to_string(agent.workspace(&session_id).unwrap().join("note.txt")).unwrap(),
        "alpha beta"
    );
    let live_files = std::fs::read_dir(agent.workspace(&session_id).unwrap().join(".zork"))
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("live-") && name.ends_with(".log"))
        })
        .collect::<Vec<_>>();
    assert_eq!(live_files.len(), 1);
    assert_eq!(
        std::fs::read_to_string(&live_files[0]).unwrap(),
        "started\nfinished\n"
    );
    after_shell
        .respond_openai_calls(
            "chatcmpl-3",
            [("provider-call-3", "file.read", json!({"path": "note.txt"}))],
        )
        .unwrap();

    let after_read = agent.request().await;
    assert!(body_contains(&after_read, "alpha beta"));
    let absolute_path = agent
        .workspace(&session_id)
        .unwrap()
        .parent()
        .unwrap()
        .join("absolute-outside-workspace.txt");
    after_read
        .respond_openai_calls(
            "chatcmpl-4",
            [
                (
                    "provider-call-parent-path",
                    "file.write",
                    json!({"path": "../parent-outside-workspace.txt", "content": "parent"}),
                ),
                (
                    "provider-call-absolute-path",
                    "file.write",
                    json!({"path": absolute_path, "content": "absolute"}),
                ),
            ],
        )
        .unwrap();
    let after_outside_writes = agent.request().await;
    assert_eq!(
        std::fs::read_to_string(
            agent
                .workspace(&session_id)
                .unwrap()
                .parent()
                .unwrap()
                .join("parent-outside-workspace.txt")
        )
        .unwrap(),
        "parent"
    );
    assert_eq!(std::fs::read_to_string(&absolute_path).unwrap(), "absolute");
    after_outside_writes
        .respond_openai_calls("chatcmpl-5", [("provider-call-5", "end", json!({}))])
        .unwrap();
    agent
        .wait_for_state(&session_id, |state| {
            state.last_turn_outcome == Some(TurnOutcome::Finished)
        })
        .await;

    let large = "0123456789".repeat(20_000);
    agent
        .send_mail(&session_id, "page and edit ordinary files")
        .await
        .unwrap();
    agent
        .request()
        .await
        .respond_openai_calls(
            "chatcmpl-file-write",
            [(
                "provider-call-file-write",
                "file.write",
                json!({"path": "large.txt", "content": large}),
            )],
        )
        .unwrap();
    let after_large_write = agent.request().await;
    assert_eq!(
        std::fs::read_to_string(agent.workspace(&session_id).unwrap().join("large.txt")).unwrap(),
        large
    );
    after_large_write
        .respond_openai_calls(
            "chatcmpl-file-read",
            [(
                "provider-call-file-read",
                "file.read",
                json!({"path": "large.txt", "offset": 10, "limit": 32}),
            )],
        )
        .unwrap();
    let after_page = agent.request().await;
    assert!(body_contains(&after_page, &large[10..42]));
    assert!(body_contains(&after_page, "next_offset"));
    after_page
        .respond_openai_calls(
            "chatcmpl-edit-seed",
            [(
                "provider-call-edit-seed",
                "file.write",
                json!({"path": "edit.txt", "content": "one\ntwo\nthree\nfour\n"}),
            )],
        )
        .unwrap();
    agent
        .request()
        .await
        .respond_openai_calls(
            "chatcmpl-file-edit",
            [(
                "provider-call-file-edit",
                "file.edit",
                json!({
                    "path": "edit.txt",
                    "edits": [
                        {"old_text": "one", "new_text": "ONE"},
                        {"old_text": "four", "new_text": "FOUR"}
                    ]
                }),
            )],
        )
        .unwrap();
    let after_edit = agent.request().await;
    assert_eq!(
        std::fs::read_to_string(agent.workspace(&session_id).unwrap().join("edit.txt")).unwrap(),
        "ONE\ntwo\nthree\nFOUR\n"
    );
    after_edit
        .respond_openai_calls(
            "chatcmpl-files-end",
            [("provider-call-files-end", "end", json!({}))],
        )
        .unwrap();
    agent
        .wait_for_state(&session_id, |state| {
            state.last_turn_outcome == Some(TurnOutcome::Finished)
        })
        .await;

    agent
        .send_mail(
            &session_id,
            "retain complete output and return only its tail",
        )
        .await
        .unwrap();
    agent
        .request()
        .await
        .respond_openai_calls(
            "chatcmpl-large-shell",
            [(
                "provider-call-large-shell",
                "shell.run",
                json!({
                    "command": "awk 'BEGIN { for (i = 0; i < 70000; i++) printf \"A\"; print \"TAIL-MARKER\" }'"
                }),
            )],
        )
        .unwrap();
    let after_large_shell = agent.request().await;
    let large_shell_body = after_large_shell.json().unwrap();
    let projected_tail = large_shell_body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "tool")
        .filter_map(|message| message["content"].as_str())
        .find(|content| content.contains("TAIL-MARKER"))
        .expect("large shell result was projected");
    assert!(projected_tail.len() < 67_000);
    assert!(projected_tail.contains("\"output_path\":\".zork/live-"));
    let large_live_output = std::fs::read_dir(agent.workspace(&session_id).unwrap().join(".zork"))
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter_map(|path| std::fs::read_to_string(path).ok())
        .find(|content| content.ends_with("TAIL-MARKER\n"))
        .expect("the complete large shell output remains in its live file");
    assert_eq!(large_live_output.len(), 70_012);
    assert!(large_live_output.starts_with("AAAA"));
    after_large_shell
        .respond_openai_calls(
            "chatcmpl-large-shell-end",
            [("provider-call-large-shell-end", "end", json!({}))],
        )
        .unwrap();
    agent
        .wait_for_state(&session_id, |state| {
            state.last_turn_outcome == Some(TurnOutcome::Finished)
        })
        .await;

    agent
        .send_mail(&session_id, "cancel the process tree")
        .await
        .unwrap();
    agent
        .request()
        .await
        .respond_openai_calls(
            "chatcmpl-shell-cancel",
            [(
                "provider-call-shell-cancel",
                "shell.run",
                json!({
                    "command": "trap '' HUP; (sleep 1; printf leaked > late.txt) & echo $! > background.pid; wait"
                }),
            )],
        )
        .unwrap();
    let background_pid =
        wait_for_file(&agent.workspace(&session_id).unwrap().join("background.pid")).await;
    assert!(background_pid.trim().parse::<u32>().is_ok());
    agent.cancel(&session_id).await.unwrap();
    agent
        .wait_for_state(&session_id, |state| {
            state.last_turn_outcome == Some(TurnOutcome::Cancelled)
        })
        .await;
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    assert!(!agent
        .workspace(&session_id)
        .unwrap()
        .join("late.txt")
        .exists());

    let before_restart = agent.history(&session_id, None, 200).unwrap();
    assert!(before_restart.iter().any(|event| matches!(
        event.event,
        SessionEvent::TurnFinished {
            outcome: TurnOutcome::Finished,
            ..
        }
    )));
    assert!(matches!(
        &before_restart[0].event,
        SessionEvent::SessionCreated {
            system_prompt: Some(prompt),
            workspace,
            ..
        } if prompt == "real environment test"
            && workspace == &agent.workspace(&session_id).unwrap().to_string_lossy()
    ));
    let messages = agent.messages(&session_id).await.unwrap();
    assert!(messages["items"]
        .as_array()
        .unwrap()
        .iter()
        .any(|message| message["content"] == "create and inspect note.txt"));

    let second_session = agent
        .create_session(
            SessionSelection {
                profile_id: "second-profile".into(),
                model: "second-model".into(),
                thinking: "low".into(),
            },
            None,
        )
        .await
        .unwrap();
    assert_ne!(
        agent.workspace(&session_id),
        agent.workspace(&second_session)
    );
    assert!(matches!(
        &agent.history(&second_session, None, 1).unwrap()[0].event,
        SessionEvent::SessionCreated {
            system_prompt: None,
            ..
        }
    ));
    agent
        .send_mail(&second_session, "independent second session")
        .await
        .unwrap();
    agent
        .request()
        .await
        .respond_openai_calls(
            "chatcmpl-second",
            [("provider-call-second", "end", json!({}))],
        )
        .unwrap();
    agent
        .wait_for_state(&second_session, |state| {
            state.last_turn_outcome == Some(TurnOutcome::Finished)
        })
        .await;

    agent.stop_agent().await;
    let segment = active_segment(agent.data_root(), &session_id);
    let segment_name = segment.file_name().unwrap().to_string_lossy();
    assert!(segment_name.ends_with(".jsonl"));
    assert_eq!(segment_name.trim_end_matches(".jsonl").len(), 16);
    std::fs::OpenOptions::new()
        .append(true)
        .open(&segment)
        .unwrap()
        .write_all(b"{\"event_id\":\"torn\"")
        .unwrap();
    agent.start_agent().await.unwrap();
    let recovered = agent.state(&session_id).await.unwrap();
    assert_eq!(recovered.last_turn_outcome, Some(TurnOutcome::Cancelled));
    assert_eq!(
        std::fs::read_to_string(agent.workspace(&session_id).unwrap().join("note.txt")).unwrap(),
        "alpha beta"
    );
    assert_eq!(
        agent
            .state(&second_session)
            .await
            .unwrap()
            .last_turn_outcome,
        Some(TurnOutcome::Finished)
    );

    agent
        .send_mail(&session_id, "continue after restart")
        .await
        .unwrap();
    let after_restart = agent.request().await;
    assert!(body_contains(&after_restart, "continue after restart"));
    after_restart
        .respond_openai_calls("chatcmpl-5", [("provider-call-5", "end", json!({}))])
        .unwrap();
    agent
        .wait_for_state(&session_id, |state| {
            state.last_turn_outcome == Some(TurnOutcome::Finished)
                && state.active_turn.is_none()
                && state.unconsumed_inputs.is_empty()
        })
        .await;

    let recovered_history = agent.history(&session_id, None, 500).unwrap();
    let recovered_ids = recovered_history
        .iter()
        .map(|event| event.event_id.as_str())
        .collect::<Vec<_>>();
    assert!(recovered_ids.iter().all(|event_id| {
        event_id.len() == 16 && event_id.bytes().all(|byte| byte.is_ascii_digit())
    }));
    assert!(recovered_ids.windows(2).all(|ids| ids[0] < ids[1]));

    let damaged_session = agent
        .create_session(
            SessionSelection {
                profile_id: "damaged-profile".into(),
                model: "damaged-model".into(),
                thinking: "medium".into(),
            },
            None,
        )
        .await
        .unwrap();
    let damaged_before = agent.history(&damaged_session, None, 10).unwrap();
    let damaged_ulid: Ulid = damaged_session.parse().unwrap();
    let last_sequence = EventId::parse(damaged_ulid, &damaged_before.last().unwrap().event_id)
        .unwrap()
        .sequence();
    let surviving_envelope = EventEnvelope {
        event_id: EventId::from_sequence(damaged_ulid, last_sequence + 2)
            .unwrap()
            .to_string(),
        schema_version: EVENT_SCHEMA_VERSION,
        batch_index: 0,
        batch_count: 1,
        event: SessionEvent::InputAppended {
            input: Input {
                input_id: Ulid::new().to_string(),
                content: "input after a damaged JSONL record".into(),
                received_at_ms: 1_700_000_000_001,
            },
        },
    };
    agent.stop_agent().await;
    let damaged_segment = active_segment(agent.data_root(), &damaged_session);
    let mut damaged_file = std::fs::OpenOptions::new()
        .append(true)
        .open(&damaged_segment)
        .unwrap();
    damaged_file.write_all(b"{not valid json}\n").unwrap();
    serde_json::to_writer(&mut damaged_file, &surviving_envelope).unwrap();
    damaged_file.write_all(b"\n{\"event_id\":\"torn\"").unwrap();
    damaged_file.sync_data().unwrap();
    drop(damaged_file);

    agent.start_agent().await.unwrap();
    let after_damage = agent.request().await;
    assert!(body_contains(
        &after_damage,
        "input after a damaged JSONL record"
    ));
    assert!(body_contains(&after_damage, "Agent runtime failure"));
    assert!(body_contains(&after_damage, "not a usable event envelope"));
    after_damage
        .respond_openai_calls(
            "chatcmpl-damaged-end",
            [("provider-call-damaged-end", "end", json!({}))],
        )
        .unwrap();
    agent
        .wait_for_state(&damaged_session, |state| {
            state.last_turn_outcome == Some(TurnOutcome::Finished)
        })
        .await;

    let workspace = agent.workspace(&session_id).unwrap().to_path_buf();
    let session_storage = agent.data_root().join("sessions").join(&session_id);
    agent.delete(&session_id).await.unwrap();
    assert!(!session_storage.exists());
    assert_eq!(
        std::fs::read_to_string(workspace.join("note.txt")).unwrap(),
        "alpha beta"
    );

    agent.shutdown().await;
    assert!(
        started.elapsed() <= Duration::from_secs(5),
        "the complete real adapter/files/shell/store/restart lifecycle took {:?}",
        started.elapsed()
    );
}
