use std::sync::Arc;
use std::time::Duration;

use ulid::Ulid;
use zork_agent::session::compression::SegmentCompressor;
use zork_agent::session::events::{Input, Selection, SessionEvent};
use zork_agent::session::query::{FileSessionQuery, SessionQuery};
use zork_agent::session::recovery::recover;
use zork_agent::session::state::{snapshot_value, STATE_SCHEMA_VERSION};
use zork_agent::session::store::{SessionStore, StoreOptions, StreamStore};
use zork_agent::session::tools::ToolRegistry;

#[test]
// Contract: docs/zork-agent-architecture.md [SEGMENT-01, SEGMENT-02, SNAPSHOT-01, QUERY-01, SSE-02]
fn snapshot_starts_a_new_segment_and_sealed_history_remains_recoverable() {
    let root = tempfile::tempdir().unwrap();
    let session_id: Ulid = "01ARZ3NDEKTSV4RRFFQ69G5FAV".parse().unwrap();
    let session_text = session_id.to_string();
    let store = StreamStore::open_with_options(
        root.path(),
        StoreOptions {
            segment_target_bytes: 1,
        },
    )
    .unwrap();
    let events = [
        SessionEvent::SessionCreated {
            session_id: session_text.clone(),
            created_at_ms: 1,
            selection: Selection {
                profile_id: "default".into(),
                model: "model-a".into(),
                thinking: "medium".into(),
            },
            system_prompt: None,
            workspace: "/workspace".into(),
            tools: Vec::new(),
        },
        SessionEvent::InputAppended {
            input: Input {
                input_id: "01BX5ZZKBKACTAV9WEVGEMMVRZ".into(),
                content: "hello".into(),
                received_at_ms: 2,
            },
        },
    ];
    let initial = store.append_batch(&session_text, &events).unwrap();
    let registry = ToolRegistry::default();
    let query = FileSessionQuery::open(root.path());
    let state = recover(&query, &session_text, None, &registry)
        .unwrap()
        .state;

    let snapshot = store
        .append_snapshot(
            &session_text,
            STATE_SCHEMA_VERSION,
            snapshot_value(&state).unwrap(),
        )
        .unwrap();
    assert_eq!(
        snapshot.sealed_segment.as_deref(),
        Some(initial[0].event_id.as_str())
    );
    let new_segment = root
        .path()
        .join("sessions")
        .join(&session_text)
        .join("segments")
        .join(format!("{}.jsonl", snapshot.envelope.event_id));
    assert!(new_segment.is_file());
    let post_snapshot = store
        .append_batch(
            &session_text,
            &[SessionEvent::SelectionChanged {
                selection: Selection {
                    profile_id: "default".into(),
                    model: "model-b".into(),
                    thinking: "medium".into(),
                },
            }],
        )
        .unwrap();

    let query = FileSessionQuery::open(root.path());
    let history = query.before(&session_text, None, 10).unwrap();
    assert_eq!(history.len(), events.len() + 1);
    assert_eq!(history[0].event, events[0]);
    assert_eq!(history[1].event, events[1]);
    assert_eq!(history[2], post_snapshot[0]);
    assert!(history[1].event_id < snapshot.envelope.event_id);
    assert!(snapshot.envelope.event_id < history[2].event_id);
    assert_eq!(
        query
            .after(&session_text, Some(&history[1].event_id), 10)
            .unwrap(),
        post_snapshot
    );

    store
        .compress_segment(&session_text, snapshot.sealed_segment.as_deref().unwrap())
        .unwrap();
    let compressed = root
        .path()
        .join("sessions")
        .join(&session_text)
        .join("segments")
        .join(format!("{}.jsonl.zst", initial[0].event_id));
    assert!(compressed.is_file());
    let buffered_before = query.before(&session_text, None, 2).unwrap();
    assert_eq!(buffered_before.len(), 2);
    assert_eq!(buffered_before[0].event, events[1]);
    assert_eq!(buffered_before[1], post_snapshot[0]);

    drop(store);
    let _reopened = StreamStore::open(root.path()).unwrap();
    let query = FileSessionQuery::open(root.path());
    let recovered = recover(&query, &session_text, None, &registry)
        .unwrap()
        .state;
    assert_eq!(recovered.session_id, session_text);
    assert_eq!(recovered.unconsumed_inputs.len(), 1);
    assert_eq!(recovered.unconsumed_inputs[0].content, "hello");
    assert_eq!(recovered.selection.unwrap().model, "model-b");
}

#[tokio::test]
// Contract: docs/zork-agent-architecture.md [SEGMENT-02]
async fn background_compression_resumes_existing_sealed_segments_and_stale_temp_files() {
    let root = tempfile::tempdir().unwrap();
    let session_id: Ulid = "01ARZ3NDEKTSV4RRFFQ69G5FAV".parse().unwrap();
    let session_text = session_id.to_string();
    let store = Arc::new(
        StreamStore::open_with_options(
            root.path(),
            StoreOptions {
                segment_target_bytes: 1,
            },
        )
        .unwrap(),
    );
    let initial = store
        .append_batch(
            &session_text,
            &[SessionEvent::SessionCreated {
                session_id: session_text.clone(),
                created_at_ms: 1,
                selection: Selection {
                    profile_id: "default".into(),
                    model: "model-a".into(),
                    thinking: "medium".into(),
                },
                system_prompt: None,
                workspace: "/workspace".into(),
                tools: Vec::new(),
            }],
        )
        .unwrap();
    let state = recover(
        &FileSessionQuery::open(root.path()),
        &session_text,
        None,
        &ToolRegistry::default(),
    )
    .unwrap()
    .state;
    store
        .append_snapshot(
            &session_text,
            STATE_SCHEMA_VERSION,
            snapshot_value(&state).unwrap(),
        )
        .unwrap();

    let segments = root
        .path()
        .join("sessions")
        .join(&session_text)
        .join("segments");
    let first_event_id = &initial[0].event_id;
    let source = segments.join(format!("{first_event_id}.jsonl"));
    let destination = segments.join(format!("{first_event_id}.jsonl.zst"));
    let temporary = segments.join(format!(".{first_event_id}.jsonl.zst.tmp"));
    std::fs::write(&temporary, b"stale partial output").unwrap();

    let compressor = SegmentCompressor::start(store, Duration::from_millis(10));
    tokio::time::timeout(Duration::from_secs(2), async {
        while !destination.is_file() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    compressor.shutdown().await;

    assert!(!source.exists());
    assert!(!temporary.exists());
}

#[test]
// Contract: docs/zork-agent-architecture.md [SNAPSHOT-03, RECOVERY-01, QUERY-01]
fn recovery_uses_an_older_snapshot_without_losing_the_newer_suffix() {
    let root = tempfile::tempdir().unwrap();
    let session_id: Ulid = "01ARZ3NDEKTSV4RRFFQ69G5FAV".parse().unwrap();
    let session_text = session_id.to_string();
    let store = StreamStore::open(root.path()).unwrap();
    let registry = ToolRegistry::default();

    store
        .append_batch(
            &session_text,
            &[SessionEvent::SessionCreated {
                session_id: session_text.clone(),
                created_at_ms: 1,
                selection: Selection {
                    profile_id: "default".into(),
                    model: "model-a".into(),
                    thinking: "medium".into(),
                },
                system_prompt: None,
                workspace: "/workspace".into(),
                tools: Vec::new(),
            }],
        )
        .unwrap();
    let query = FileSessionQuery::open(root.path());
    let state = recover(&query, &session_text, None, &registry)
        .unwrap()
        .state;
    store
        .append_snapshot(
            &session_text,
            STATE_SCHEMA_VERSION,
            snapshot_value(&state).unwrap(),
        )
        .unwrap();
    store
        .append_batch(
            &session_text,
            &[SessionEvent::SelectionChanged {
                selection: Selection {
                    profile_id: "default".into(),
                    model: "model-b".into(),
                    thinking: "medium".into(),
                },
            }],
        )
        .unwrap();
    store
        .append_snapshot(
            &session_text,
            STATE_SCHEMA_VERSION + 1,
            serde_json::json!({}),
        )
        .unwrap();
    store
        .append_batch(
            &session_text,
            &[SessionEvent::SelectionChanged {
                selection: Selection {
                    profile_id: "default".into(),
                    model: "model-c".into(),
                    thinking: "medium".into(),
                },
            }],
        )
        .unwrap();

    let recovered = recover(&query, &session_text, None, &registry).unwrap();
    assert_eq!(recovered.state.selection.unwrap().model, "model-c");
    assert!(recovered
        .diagnostics
        .iter()
        .any(|message| message.contains("was not usable")));
}
