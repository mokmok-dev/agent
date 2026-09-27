//! Unit tests for the `CloudEvents` envelope: the worked examples from the design
//! docs, the bus-owned attribute rules, and the fixed-width `sequence` encoding.

use serde_json::json;

use super::*;

/// A fixed commit time so normalization is deterministic.
fn epoch() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(0).expect("the Unix epoch is representable")
}

#[test]
fn the_documented_delivered_envelope_round_trips() {
    // The delivered-event example from docs/event-bus/protocol.md.
    let json = br#"{"specversion":"1.0","type":"agent.task.started",
        "source":"agent://eventbus","id":"01J8Z...","time":"2026-09-27T00:00:00Z",
        "sequence":"00000000000000001024","data":{"task_id":"t-1"}}"#;

    let event = Event::from_bytes(json).expect("a valid delivered event");
    assert_eq!(event.ty, "agent.task.started");
    assert_eq!(event.source, "agent://eventbus");
    assert_eq!(event.sequence.get(), 1024);
    assert_eq!(event.data, Some(json!({"task_id": "t-1"})));

    let bytes = event.to_bytes().expect("a parsed event re-serializes");
    let reparsed = Event::from_bytes(&bytes).expect("its own bytes re-parse");
    assert_eq!(reparsed, event);
}

#[test]
fn a_sandbox_extension_attribute_survives_the_round_trip() {
    // The egress-request example from docs/sandbox/events.md, carrying
    // `traceparent` as an extension. The doc elides the bus-assigned `sequence`,
    // which a persisted event always carries, so it is added here.
    let json = br#"{"specversion":"1.0","type":"agent.sandbox.egress.requested",
        "source":"agent://eventbus","id":"01J8Z...","subject":"sbx-7",
        "time":"2026-09-27T00:00:00Z","sequence":"00000000000000001024",
        "traceparent":"00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
        "data":{"request_id":"req-9","host":"api.example.com","port":443}}"#;

    let event = Event::from_bytes(json).expect("a valid sandbox event");
    assert_eq!(event.subject.as_deref(), Some("sbx-7"));
    assert_eq!(
        event.extensions.get("traceparent"),
        Some(&json!(
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
        ))
    );

    let bytes = event.to_bytes().expect("serializes");
    assert_eq!(Event::from_bytes(&bytes).expect("reparses"), event);
}

#[test]
fn commit_assigns_every_bus_owned_attribute() {
    let json = br#"{"specversion":"1.0","type":"agent.task.started",
        "data":{"task_id":"t-1"}}"#;
    let incoming: Incoming = serde_json::from_slice(json).expect("a valid incoming event");

    let event = incoming
        .commit("agent://eventbus", Sequence::new(7), epoch())
        .expect("no reserved attribute was set");

    assert_eq!(event.source, "agent://eventbus");
    assert_eq!(event.sequence, Sequence::new(7));
    assert_eq!(event.time, Some(epoch()));
    // A ULID is 26 Crockford base32 characters.
    assert_eq!(event.id.len(), 26);
}

#[test]
fn commit_keeps_a_producer_supplied_id_and_time() {
    let json = br#"{"specversion":"1.0","type":"agent.task.started",
        "id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","time":"2020-01-02T03:04:05Z"}"#;
    let incoming: Incoming = serde_json::from_slice(json).expect("a valid incoming event");
    let producer_time =
        OffsetDateTime::from_unix_timestamp(1_577_934_245).expect("a representable time");

    let event = incoming
        .commit("agent://eventbus", Sequence::new(1), epoch())
        .expect("no reserved attribute was set");

    assert_eq!(event.id, "01ARZ3NDEKTSV4RRFFQ69G5FAV");
    assert_eq!(event.time, Some(producer_time));
}

#[test]
fn commit_rejects_a_producer_supplied_source() {
    let json = br#"{"specversion":"1.0","type":"e","source":"agent://forged"}"#;
    let incoming: Incoming = serde_json::from_slice(json).expect("a valid incoming event");

    let error = incoming
        .commit("agent://eventbus", Sequence::new(1), epoch())
        .expect_err("a producer must not set source");
    assert!(matches!(error, Error::ReservedAttribute("source")));
}

#[test]
fn commit_rejects_a_producer_supplied_sequence() {
    let json = br#"{"specversion":"1.0","type":"e","sequence":"00000000000000000001"}"#;
    let incoming: Incoming = serde_json::from_slice(json).expect("a valid incoming event");

    let error = incoming
        .commit("agent://eventbus", Sequence::new(1), epoch())
        .expect_err("a producer must not set sequence");
    assert!(matches!(error, Error::ReservedAttribute("sequence")));
}

#[test]
fn a_sequence_is_twenty_zero_padded_digits() {
    assert_eq!(Sequence::new(0).to_string(), "00000000000000000000");
    assert_eq!(Sequence::new(1024).to_string(), "00000000000000001024");
    assert_eq!(Sequence::new(u64::MAX).to_string(), "18446744073709551615");
}

#[test]
fn a_sequence_parses_only_its_canonical_form() {
    let canonical: Sequence = "00000000000000001024".parse().expect("canonical form");
    assert_eq!(canonical, Sequence::new(1024));

    // Too short, too long, non-digit, and overflowing inputs are rejected.
    assert!(matches!(
        "1024".parse::<Sequence>(),
        Err(Error::InvalidSequence(_))
    ));
    assert!(matches!(
        "000000000000000010240".parse::<Sequence>(),
        Err(Error::InvalidSequence(_))
    ));
    assert!(matches!(
        "0000000000000000102a".parse::<Sequence>(),
        Err(Error::InvalidSequence(_))
    ));
    assert!(matches!(
        "99999999999999999999".parse::<Sequence>(),
        Err(Error::InvalidSequence(_))
    ));
}

#[test]
fn serialization_is_deterministic_regardless_of_extension_order() {
    // The same two extensions supplied in opposite orders must produce the same
    // bytes, because serialization is hashed and chained. A fixed `id` keeps the
    // two events otherwise identical despite `Ulid::new` being random.
    let json_a = br#"{"specversion":"1.0","type":"e",
        "id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","a":"1","b":"2"}"#;
    let json_b = br#"{"specversion":"1.0","type":"e",
        "id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","b":"2","a":"1"}"#;
    let event_a: Incoming = serde_json::from_slice(json_a).expect("valid");
    let event_b: Incoming = serde_json::from_slice(json_b).expect("valid");

    let committed_a = event_a
        .commit("agent://eventbus", Sequence::new(1), epoch())
        .expect("commits");
    let committed_b = event_b
        .commit("agent://eventbus", Sequence::new(1), epoch())
        .expect("commits");

    assert_eq!(committed_a, committed_b);
    assert_eq!(
        committed_a.to_bytes().expect("serializes"),
        committed_b.to_bytes().expect("serializes")
    );
}

#[test]
fn a_non_1_0_specversion_is_rejected() {
    let json = br#"{"specversion":"0.3","type":"agent.task.started"}"#;
    let error = Event::from_bytes(json).expect_err("only CloudEvents 1.0 is modeled");
    assert!(matches!(error, Error::Json(_)));
}

#[test]
fn an_event_missing_a_required_attribute_is_rejected() {
    // `type` is required; `specversion` alone is not an event.
    let json = br#"{"specversion":"1.0"}"#;
    assert!(matches!(Event::from_bytes(json), Err(Error::Json(_))));
}

#[test]
fn an_event_without_a_time_omits_it_when_serialized() {
    let event = Event {
        specversion: SpecVersion::V1_0,
        ty: "e".to_owned(),
        source: "agent://eventbus".to_owned(),
        id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".to_owned(),
        time: None,
        subject: None,
        datacontenttype: None,
        sequence: Sequence::new(1),
        data: None,
        extensions: BTreeMap::new(),
    };

    let value = serde_json::to_value(&event).expect("serializes");
    assert!(value.get("time").is_none());
}
