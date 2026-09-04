use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};

use ulid::Ulid;
use zork_agent::session::event_id::EventId;
use zork_agent::session::events::{
    Input, Selection, SessionEvent, TurnOutcome, EVENT_SCHEMA_VERSION,
};
use zork_agent::session::query::{FileSessionQuery, SessionQuery};
use zork_agent::session::recovery::recover;
use zork_agent::session::state::{snapshot_value, STATE_SCHEMA_VERSION};
use zork_agent::session::store::{EventEnvelope, SessionStore, StreamStore};
use zork_agent::session::tools::ToolRegistry;

fn selection(model: &str) -> SessionEvent {
    SessionEvent::SelectionChanged {
        selection: Selection {
            profile_id: "default".into(),
            model: model.into(),
            thinking: "medium".into(),
        },
    }
}

#[test]
// Contract: docs/zork-agent-architecture.md [EVENT-01, EVENT-03]
fn complete_batches_round_trip_and_sequence_continues_after_reopen() {
    let root = tempfile::tempdir().unwrap();
    let session_id: Ulid = "01ARZ3NDEKTSV4RRFFQ69G5FAV".parse().unwrap();
    let session_text = session_id.to_string();
    let events = [selection("model-a"), selection("model-b")];

    let store = StreamStore::open(root.path()).unwrap();
    let persisted = store.append_batch(&session_text, &events).unwrap();
    assert_eq!(persisted.len(), events.len());
    for (index, envelope) in persisted.iter().enumerate() {
        assert_eq!(envelope.schema_version, EVENT_SCHEMA_VERSION);
        assert_eq!(envelope.batch_index, index as u32);
        assert_eq!(envelope.batch_count, events.len() as u32);
        assert_eq!(envelope.event, events[index]);
        assert_eq!(
            EventId::parse(session_id, &envelope.event_id)
                .unwrap()
                .sequence(),
            index as u64 + 1
        );
    }

    let segment = root
        .path()
        .join("sessions")
        .join(&session_text)
        .join("segments")
        .join(format!("{}.jsonl", persisted[0].event_id));
    let stored: Vec<EventEnvelope> = BufReader::new(File::open(segment).unwrap())
        .lines()
        .map(|line| serde_json::from_str(&line.unwrap()).unwrap())
        .collect();
    assert_eq!(stored, persisted);

    drop(store);
    let reopened = StreamStore::open(root.path()).unwrap();
    let next = reopened
        .append_batch(&session_text, &[selection("model-c")])
        .unwrap();
    assert_eq!(next.len(), 1);
    assert_eq!(
        EventId::parse(session_id, &next[0].event_id)
            .unwrap()
            .sequence(),
        3
    );
}

#[test]
// Contract: docs/zork-agent-architecture.md [PROJECTION-02]
fn recovery_preserves_domain_ulids_independently_from_event_ids() {
    let root = tempfile::tempdir().unwrap();
    let session_id: Ulid = "01ARZ3NDEKTSV4RRFFQ69G5FAV".parse().unwrap();
    let session_text = session_id.to_string();
    let input_id = "01BX5ZZKBKACTAV9WEVGEMMVRZ";
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
                input_id: input_id.into(),
                content: "hello".into(),
                received_at_ms: 2,
            },
        },
    ];

    let store = StreamStore::open(root.path()).unwrap();
    store.append_batch(&session_text, &events).unwrap();
    let query = FileSessionQuery::open(root.path());
    let recovered = recover(&query, &session_text, None, &ToolRegistry::default())
        .unwrap()
        .state;
    assert_eq!(recovered.session_id, session_text);
    assert_eq!(recovered.unconsumed_inputs.len(), 1);
    assert_eq!(recovered.unconsumed_inputs[0].input_id, input_id);
    assert_eq!(recovered.unconsumed_inputs[0].content, "hello");
}

#[test]
// Contract: docs/zork-agent-architecture.md [PERSIST-01]
fn data_root_ownership_is_exclusive_and_released_with_the_store() {
    let first_root = tempfile::tempdir().unwrap();
    let second_root = tempfile::tempdir().unwrap();

    let owner = StreamStore::open(first_root.path()).unwrap();
    let independent = StreamStore::open(second_root.path()).unwrap();
    assert!(StreamStore::open(first_root.path()).is_err());

    drop(owner);
    let successor = StreamStore::open(first_root.path()).unwrap();
    drop((successor, independent));
}

#[test]
// Contract: docs/zork-agent-architecture.md [STARTUP-03]
fn last_commit_read_returns_a_complete_normally_finished_commit() {
    let root = tempfile::tempdir().unwrap();
    let session_id: Ulid = "01ARZ3NDEKTSV4RRFFQ69G5FAV".parse().unwrap();
    let session_text = session_id.to_string();
    let store = StreamStore::open(root.path()).unwrap();
    store
        .append_batch(
            &session_text,
            &[
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
                SessionEvent::TurnStarted {
                    turn_id: "turn-a".into(),
                    started_at_ms: 2,
                },
                SessionEvent::TurnFinished {
                    turn_id: "turn-a".into(),
                    outcome: TurnOutcome::Finished,
                    outstanding: Vec::new(),
                    finished_at_ms: 3,
                },
            ],
        )
        .unwrap();

    let commit = FileSessionQuery::open(root.path())
        .last_commit(&session_text, None)
        .unwrap()
        .value
        .unwrap();
    assert!(matches!(
        commit.last().map(|envelope| &envelope.event),
        Some(SessionEvent::TurnFinished {
            outcome: TurnOutcome::Finished,
            ..
        })
    ));
}

#[test]
// Contract: docs/zork-agent-architecture.md [EVENT-02, SEGMENT-01]
fn recovery_and_compression_keep_complete_batches_and_drop_only_an_incomplete_tail_batch() {
    let root = tempfile::tempdir().unwrap();
    let session_id: Ulid = "01ARZ3NDEKTSV4RRFFQ69G5FAV".parse().unwrap();
    let session_text = session_id.to_string();
    let store = StreamStore::open_with_options(
        root.path(),
        zork_agent::session::store::StoreOptions {
            segment_target_bytes: 1,
        },
    )
    .unwrap();
    let created = SessionEvent::SessionCreated {
        session_id: session_text.clone(),
        created_at_ms: 1,
        selection: Selection {
            profile_id: "default".into(),
            model: "original".into(),
            thinking: "medium".into(),
        },
        system_prompt: None,
        workspace: "/workspace".into(),
        tools: Vec::new(),
    };
    let first = store.append_batch(&session_text, &[created]).unwrap();
    store
        .append_batch(
            &session_text,
            &[selection("partial-a"), selection("partial-b")],
        )
        .unwrap();

    let active = root
        .path()
        .join("sessions")
        .join(&session_text)
        .join("segments")
        .join(format!("{}.jsonl", first[0].event_id));
    remove_last_jsonl_record(&active);
    drop(store);

    let reopened = StreamStore::open_with_options(
        root.path(),
        zork_agent::session::store::StoreOptions {
            segment_target_bytes: 1,
        },
    )
    .unwrap();
    let registry = ToolRegistry::default();
    let query = FileSessionQuery::open(root.path());
    let recovered = recover(&query, &session_text, None, &registry)
        .unwrap()
        .state;
    assert_eq!(recovered.selection.as_ref().unwrap().model, "original");

    let snapshot = reopened
        .append_snapshot(
            &session_text,
            STATE_SCHEMA_VERSION,
            snapshot_value(&recovered).unwrap(),
        )
        .unwrap();
    let sealed = snapshot.sealed_segment.unwrap();
    reopened.compress_segment(&session_text, &sealed).unwrap();

    let visible = FileSessionQuery::open(root.path())
        .after(&session_text, None, 10)
        .unwrap();
    assert_eq!(visible.len(), 1);
    assert!(matches!(
        visible[0].event,
        SessionEvent::SessionCreated { .. }
    ));
}

fn remove_last_jsonl_record(path: &std::path::Path) {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut file, &mut bytes).unwrap();
    assert_eq!(bytes.last(), Some(&b'\n'));
    let previous_newline = bytes[..bytes.len() - 1]
        .iter()
        .rposition(|byte| *byte == b'\n')
        .unwrap();
    file.set_len((previous_newline + 1) as u64).unwrap();
    file.seek(SeekFrom::Start((previous_newline + 1) as u64))
        .unwrap();
    file.flush().unwrap();
}
