//! Property tests for the `CloudEvents` envelope. The properties that matter
//! for durability are that the fixed-width `sequence` encoding preserves numeric
//! order and that an event survives a serialize/parse round-trip unchanged.

use agent::cloudevent::{Event, Incoming, SEQUENCE_WIDTH, Sequence, SpecVersion};
use proptest::prelude::*;

/// A JSON value from the subset the bus accepts as `data`.
fn json_value() -> impl Strategy<Value = serde_json::Value> {
    prop_oneof![
        any::<bool>().prop_map(serde_json::Value::from),
        any::<i64>().prop_map(serde_json::Value::from),
        any::<String>().prop_map(serde_json::Value::from),
    ]
}

proptest! {
    /// The 20-digit decimal encoding is width-preserving, so lexicographic
    /// order equals numeric order over the whole `u64` range. This is what the
    /// `CloudEvents` Sequence extension's ordering requirement rests on.
    #[test]
    fn sequence_encoding_preserves_numeric_order(a in any::<u64>(), b in any::<u64>()) {
        let left = Sequence::new(a).to_string();
        let right = Sequence::new(b).to_string();

        prop_assert_eq!(left.len(), SEQUENCE_WIDTH);
        prop_assert_eq!(right.len(), SEQUENCE_WIDTH);
        prop_assert_eq!(a.cmp(&b), left.cmp(&right));
    }

    /// A sequence renders and parses back to the same value.
    #[test]
    fn a_sequence_round_trips(value in any::<u64>()) {
        let text = Sequence::new(value).to_string();
        prop_assert_eq!(text.parse::<Sequence>().unwrap(), Sequence::new(value));
    }

    /// Committing an event and re-parsing its bytes reproduces the same event.
    #[test]
    fn a_committed_event_round_trips(
        ty in "[a-z][a-z0-9.]{0,32}",
        subject in proptest::option::of("[a-z0-9-]{0,16}"),
        data in proptest::option::of(json_value()),
        sequence in any::<u64>(),
    ) {
        let incoming = Incoming {
            specversion: SpecVersion::V1_0,
            ty,
            source: None,
            id: Some("01ARZ3NDEKTSV4RRFFQ69G5FAV".to_owned()),
            time: Some(time::OffsetDateTime::from_unix_timestamp(0).unwrap()),
            subject,
            datacontenttype: None,
            sequence: None,
            data,
            extensions: std::collections::BTreeMap::new(),
        };
        let committed = incoming
            .commit(
                "agent://eventbus",
                Sequence::new(sequence),
                time::OffsetDateTime::from_unix_timestamp(0).unwrap(),
            )
            .unwrap();

        let bytes = committed.to_bytes().unwrap();
        let reparsed = Event::from_bytes(&bytes).unwrap();
        prop_assert_eq!(reparsed, committed);
    }
}
