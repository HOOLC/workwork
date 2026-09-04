use ulid::Ulid;
use zork_agent::session::event_id::EventId;

#[test]
// Contract: docs/zork-agent-architecture.md [EVENT-03]
fn canonical_event_ids_are_stable_round_trippable_and_ordered() {
    let session_id: Ulid = "01ARZ3NDEKTSV4RRFFQ69G5FAV".parse().unwrap();
    let expected = [
        (1, "0000000000000145"),
        (2, "0000000000000221"),
        (9, "0000000000000982"),
        (10, "0000000000001087"),
        (99_999_999_999_999, "9999999999999923"),
    ];

    let mut encoded = Vec::new();
    for (sequence, text) in expected {
        let event_id = EventId::from_sequence(session_id, sequence).unwrap();
        assert_eq!(event_id.as_str(), text);
        assert_eq!(event_id.sequence(), sequence);
        assert_eq!(EventId::parse(session_id, text).unwrap(), event_id);
        assert_eq!(event_id.as_str().len(), 16);
        assert!(event_id.as_str().bytes().all(|byte| byte.is_ascii_digit()));
        encoded.push(event_id);
    }

    assert!(encoded.windows(2).all(|pair| pair[0] < pair[1]));
}
