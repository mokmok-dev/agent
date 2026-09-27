//! Tests for the wire protocol: the classification rule, the documented
//! exchange, and the round-trip of every control message.

use super::*;

#[test]
fn a_publish_request_round_trips() {
    // The publish example from docs/event-bus/protocol.md.
    let json = r#"{"type":"publish","id":"req-1","event":{
        "specversion":"1.0","type":"agent.task.started",
        "source":"agent://eventbus","id":"01J8Z...","time":"2026-09-27T00:00:00Z",
        "data":{"task_id":"t-1"}}}"#;

    let message = parse(json).expect("a valid publish request");
    let Message::Client(ClientMessage::Publish { id, event, .. }) = message else {
        panic!("expected a publish request, got {message:?}");
    };
    assert_eq!(id, "req-1");
    assert_eq!(event.ty, "agent.task.started");
    assert_eq!(event.id.as_deref(), Some("01J8Z..."));
}

#[test]
fn the_documented_reply_round_trips() {
    let json = r#"{"type":"published","id":"req-1","seq":1024}"#;
    let message = parse(json).expect("a valid reply");
    assert_eq!(
        message,
        Message::Server(ServerMessage::Published {
            id: "req-1".to_owned(),
            seq: 1024,
        })
    );
}

#[test]
fn a_subscribe_request_round_trips() {
    let message = parse(r#"{"type":"subscribe","from_seq":1000}"#).expect("valid");
    assert_eq!(
        message,
        Message::Client(ClientMessage::Subscribe {
            from_seq: 1000,
            filter: None,
        })
    );
}

#[test]
fn an_ack_round_trips() {
    let message = parse(r#"{"type":"ack","cursor":1024}"#).expect("valid");
    assert_eq!(
        message,
        Message::Client(ClientMessage::Ack { cursor: 1024 })
    );
}

#[test]
fn a_delivered_event_is_classified_as_an_event() {
    // The delivered-event example from docs/event-bus/protocol.md.
    let json = r#"{"specversion":"1.0","type":"agent.task.started",
        "source":"agent://eventbus","id":"01J8Z...","time":"2026-09-27T00:00:00Z",
        "sequence":"00000000000000001024","data":{"task_id":"t-1"}}"#;

    let message = parse(json).expect("a valid event");
    let Message::Event(event) = message else {
        panic!("an object with specversion is an event, got {message:?}");
    };
    assert_eq!(event.sequence.get(), 1024);
}

#[test]
fn every_server_message_round_trips() {
    let messages = [
        ServerMessage::Published {
            id: "r".to_owned(),
            seq: 1,
        },
        ServerMessage::Subscribed { from_seq: 2 },
        ServerMessage::CursorAck { cursor: 3 },
        ServerMessage::Gap { from: 4, to: 5 },
        ServerMessage::Error {
            code: ErrorCode::Forbidden,
            message: "no".to_owned(),
            id: Some("r".to_owned()),
        },
    ];

    for message in messages {
        let text = to_text(&message).expect("serializes");
        assert_eq!(
            parse(&text).expect("reparses"),
            Message::Server(message.clone()),
            "round-trip failed for {message:?}",
        );
    }
}

#[test]
fn a_publish_with_an_idempotency_key_round_trips() {
    let json = r#"{"type":"publish","id":"r","idempotency_key":"k",
        "event":{"specversion":"1.0","type":"e"}}"#;
    let Message::Client(ClientMessage::Publish {
        idempotency_key, ..
    }) = parse(json).expect("valid")
    else {
        panic!("expected a publish request");
    };
    assert_eq!(idempotency_key.as_deref(), Some("k"));
}

#[test]
fn an_error_code_is_snake_case_on_the_wire() {
    let text = to_text(&ErrorCode::ReservedAttribute).expect("serializes");
    assert_eq!(text, "\"reserved_attribute\"");
    assert_eq!(
        serde_json::from_str::<ErrorCode>(&text).expect("reparses"),
        ErrorCode::ReservedAttribute
    );
}

#[test]
fn a_non_object_message_is_rejected() {
    assert!(matches!(parse("42"), Err(Error::NotAnObject)));
    assert!(matches!(parse("[1,2]"), Err(Error::NotAnObject)));
    assert!(matches!(parse("not json"), Err(Error::Json(_))));
}

#[test]
fn an_unknown_control_type_is_rejected() {
    assert!(matches!(
        parse(r#"{"type":"nonsense"}"#),
        Err(Error::Json(_))
    ));
}
