use ulid::Ulid;
use zork_agent::session::event_id::EventId;
use zork_agent::session::events::{Selection, SessionEvent};
use zork_agent::session::query::{FileSessionQuery, QueryError, SessionQuery, WindowOrigin};
use zork_agent::session::store::{SessionStore, StoreOptions, StreamStore};

fn selection(model: &str) -> SessionEvent {
    SessionEvent::SelectionChanged {
        selection: Selection {
            profile_id: "default".into(),
            model: model.into(),
            thinking: "medium".into(),
        },
    }
}

fn jsonl_segment(root: &std::path::Path, session_id: &str) -> std::path::PathBuf {
    let segments = root.join("sessions").join(session_id).join("segments");
    std::fs::read_dir(segments)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.to_string_lossy().ends_with(".jsonl"))
        .unwrap()
}

fn replace_event(root: &std::path::Path, session_id: &str, index: usize, event: serde_json::Value) {
    let path = jsonl_segment(root, session_id);
    let mut records = std::fs::read_to_string(&path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    records[index]["event"] = event;
    let mut encoded = records
        .iter()
        .map(|record| serde_json::to_string(record).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    encoded.push('\n');
    std::fs::write(path, encoded).unwrap();
}

fn retain_record_prefix(root: &std::path::Path, session_id: &str, count: usize) {
    let path = jsonl_segment(root, session_id);
    let mut encoded = std::fs::read_to_string(&path)
        .unwrap()
        .lines()
        .take(count)
        .collect::<Vec<_>>()
        .join("\n");
    encoded.push('\n');
    std::fs::write(path, encoded).unwrap();
}

fn replace_event_id(path: &std::path::Path, index: usize, event_id: &str) {
    let mut records = std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    records[index]["event_id"] = serde_json::Value::String(event_id.to_owned());
    let mut encoded = records
        .iter()
        .map(|record| serde_json::to_string(record).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    encoded.push('\n');
    std::fs::write(path, encoded).unwrap();
}

#[test]
// Contract: docs/zork-agent-architecture.md [QUERY-01, SNAPSHOT-03]
fn query_pages_and_locates_persisted_events_by_durable_cursor() {
    let root = tempfile::tempdir().unwrap();
    let session_id: Ulid = "01ARZ3NDEKTSV4RRFFQ69G5FAV".parse().unwrap();
    let session_text = session_id.to_string();
    let store = StreamStore::open(root.path()).unwrap();
    let envelopes = store
        .append_batch(
            &session_text,
            &[
                selection("model-a"),
                selection("model-b"),
                selection("model-c"),
            ],
        )
        .unwrap();

    let query = FileSessionQuery::open(root.path());
    let after = query
        .after(&session_text, Some(&envelopes[0].event_id), 10)
        .unwrap();
    assert_eq!(
        after
            .iter()
            .map(|event| event.event_id.as_str())
            .collect::<Vec<_>>(),
        vec![
            envelopes[1].event_id.as_str(),
            envelopes[2].event_id.as_str()
        ]
    );

    let before = query
        .before(&session_text, Some(&envelopes[2].event_id), 2)
        .unwrap();
    assert_eq!(
        before
            .iter()
            .map(|event| event.event_id.as_str())
            .collect::<Vec<_>>(),
        vec![
            envelopes[0].event_id.as_str(),
            envelopes[1].event_id.as_str()
        ]
    );

    let found = query
        .event(&session_text, &envelopes[1].event_id)
        .unwrap()
        .unwrap();
    assert_eq!(found, envelopes[1]);
    assert_eq!(query.before(&session_text, None, 10).unwrap(), envelopes);
    assert_eq!(
        query
            .after(&session_text, Some(&envelopes[0].event_id), 1)
            .unwrap(),
        vec![envelopes[1].clone()]
    );
    assert_eq!(
        query.event(&session_text, &envelopes[1].event_id).unwrap(),
        Some(envelopes[1].clone())
    );

    store
        .append_snapshot(&session_text, 1, serde_json::json!({"state": "fixture"}))
        .unwrap();
    let post_snapshot = store
        .append_batch(&session_text, &[selection("model-d")])
        .unwrap();

    let last = query.last_commit(&session_text, None).unwrap();
    assert!(last.diagnostics.is_empty());
    assert_eq!(last.value.unwrap().events, post_snapshot);
    let refreshed = query.before(&session_text, None, 10).unwrap();
    assert_eq!(refreshed.len(), 4);
    assert_eq!(refreshed.last(), post_snapshot.last());
    assert_eq!(
        query
            .after(&session_text, Some(&envelopes[2].event_id), 10)
            .unwrap(),
        post_snapshot
    );

    let mut window = None;
    let summary = query
        .snapshot_windows(&session_text, None, &mut |value| {
            window = Some(value);
            true
        })
        .unwrap();
    assert!(summary.diagnostics.is_empty());
    let window = window.unwrap();
    assert!(matches!(window.origin, WindowOrigin::Snapshot(_)));
    assert_eq!(window.commits.len(), 1);
    assert_eq!(window.commits[0].events, post_snapshot);

    let mut commits = Vec::new();
    query
        .all_commits_forward(&session_text, &mut |commit| {
            commits.push(commit);
            false
        })
        .unwrap();
    assert_eq!(commits.len(), 3);
}

#[test]
// Contract: docs/zork-agent-architecture.md [QUERY-01, SSE-01]
fn scan_after_streams_visible_history_across_segments_without_paging() {
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
    let first = store
        .append_batch(
            &session_text,
            &[
                selection("model-a"),
                selection("model-b"),
                selection("model-c"),
            ],
        )
        .unwrap();
    store
        .append_snapshot(&session_text, 1, serde_json::json!({"state": "fixture"}))
        .unwrap();
    let second = store
        .append_batch(&session_text, &[selection("model-d"), selection("model-e")])
        .unwrap();

    let query = FileSessionQuery::open(root.path());
    let mut streamed = Vec::new();
    query
        .scan_after(&session_text, Some(&first[0].event_id), &mut |event| {
            streamed.push(event);
            false
        })
        .unwrap();

    assert_eq!(streamed, [first[1..].to_vec(), second].concat());
}

#[test]
// Contract: docs/zork-agent-architecture.md [EVENT-04, QUERY-01]
fn history_validates_events_that_are_not_returned() {
    for invalid_event in [
        serde_json::json!({
            "kind": "input_appended",
            "input": {"input_id": "broken", "received_at_ms": 0}
        }),
        serde_json::json!({"kind": "snapshot"}),
    ] {
        let root = tempfile::tempdir().unwrap();
        let session_id: Ulid = "01ARZ3NDEKTSV4RRFFQ69G5FAV".parse().unwrap();
        let session_text = session_id.to_string();
        let store = StreamStore::open(root.path()).unwrap();
        let envelopes = store
            .append_batch(
                &session_text,
                &[
                    selection("model-a"),
                    selection("model-b"),
                    selection("model-c"),
                ],
            )
            .unwrap();
        replace_event(root.path(), &session_text, 0, invalid_event);

        let query = FileSessionQuery::open(root.path());
        assert!(matches!(
            query.before(&session_text, None, 1),
            Err(QueryError::InvalidLog(_))
        ));
        assert!(matches!(
            query.after(&session_text, Some(&envelopes[0].event_id), 1),
            Err(QueryError::InvalidLog(_))
        ));
    }
}

#[test]
// Contract: docs/zork-agent-architecture.md [EVENT-02, QUERY-01]
fn history_ignores_invalid_payload_in_an_incomplete_tail_batch() {
    let root = tempfile::tempdir().unwrap();
    let session_id: Ulid = "01ARZ3NDEKTSV4RRFFQ69G5FAV".parse().unwrap();
    let session_text = session_id.to_string();
    let store = StreamStore::open(root.path()).unwrap();
    store
        .append_batch(
            &session_text,
            &[
                selection("model-a"),
                selection("model-b"),
                selection("model-c"),
            ],
        )
        .unwrap();
    replace_event(
        root.path(),
        &session_text,
        0,
        serde_json::json!({
            "kind": "input_appended",
            "input": {"input_id": "broken", "received_at_ms": 0}
        }),
    );
    retain_record_prefix(root.path(), &session_text, 2);

    let query = FileSessionQuery::open(root.path());
    assert!(query.before(&session_text, None, 10).unwrap().is_empty());
    assert!(query.after(&session_text, None, 10).unwrap().is_empty());
    let mut commits = Vec::new();
    query
        .all_commits_forward(&session_text, &mut |commit| {
            commits.push(commit);
            false
        })
        .unwrap();
    assert!(commits.is_empty());
}

#[test]
// Contract: docs/zork-agent-architecture.md [EVENT-03, SEGMENT-01, QUERY-01]
fn history_rejects_event_order_that_overlaps_the_next_segment() {
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
    let initial = store
        .append_batch(&session_text, &[selection("model-a"), selection("model-b")])
        .unwrap();
    store
        .append_snapshot(&session_text, 1, serde_json::json!({"state": "fixture"}))
        .unwrap();
    store
        .append_batch(&session_text, &[selection("model-c")])
        .unwrap();

    let older_segment = root
        .path()
        .join("sessions")
        .join(&session_text)
        .join("segments")
        .join(format!("{}.jsonl", initial[0].event_id));
    for (index, sequence) in [4, 5].into_iter().enumerate() {
        let overlapping = EventId::from_sequence(session_id, sequence)
            .unwrap()
            .to_string();
        replace_event_id(&older_segment, index, &overlapping);
    }

    let query = FileSessionQuery::open(root.path());
    assert!(matches!(
        query.before(&session_text, None, 10),
        Err(QueryError::InvalidLog(_))
    ));
}
