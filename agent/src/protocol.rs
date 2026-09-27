//! Wire protocol messages.
//!
//! Implements the message schemas from `docs/event-bus/protocol.md`. Every
//! application message is a WebSocket text frame carrying JSON. A message is
//! classified by whether its top-level object carries a `specversion` field:
//!
//! - A message with `specversion` is a `CloudEvents` envelope (an event).
//! - A message without `specversion` is a control message.
//!
//! This module models the control messages and the classification. It does not
//! implement the request handlers; that is the server milestone.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::cloudevent::Incoming;

/// A control message sent from a client to the bus.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    /// Append an event to the log.
    Publish {
        /// The request correlation ID, echoed in the reply.
        id: String,
        /// The event to append.
        event: Box<Incoming>,
        /// An optional producer key a consumer can deduplicate on.
        #[serde(skip_serializing_if = "Option::is_none")]
        idempotency_key: Option<String>,
    },
    /// Begin delivery at `from_seq` (or the stored cursor if greater).
    Subscribe {
        /// The stable subscriber identity the durable cursor is keyed on.
        ///
        /// `docs/event-bus/delivery.md` requires a client-supplied subscriber
        /// ID namespaced by peer UID; the `subscribe` message is where it is
        /// supplied, and later `ack`s on the same connection refer to it.
        subscriber_id: String,
        /// The first sequence the client wants to receive.
        from_seq: u64,
        /// An optional filter the bus applies to delivery.
        #[serde(skip_serializing_if = "Option::is_none")]
        filter: Option<Value>,
    },
    /// Durably record delivery progress.
    Ack {
        /// The last sequence the client has confirmed processing.
        cursor: u64,
    },
}

/// A control message sent from the bus to a client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    /// The event was committed at `seq`.
    Published {
        /// The correlation ID from the `publish` request.
        id: String,
        /// The assigned sequence number.
        seq: u64,
    },
    /// A subscription was accepted; delivery starts at `from_seq`.
    Subscribed {
        /// The sequence delivery starts at, after the stored cursor is applied.
        from_seq: u64,
    },
    /// The cursor was durably recorded.
    CursorAck {
        /// The recorded cursor.
        cursor: u64,
    },
    /// The requested range is no longer available locally.
    Gap {
        /// The first missing sequence.
        from: u64,
        /// The last missing sequence.
        to: u64,
    },
    /// A request failed.
    Error {
        /// A machine-readable error code.
        code: ErrorCode,
        /// A human-readable message.
        message: String,
        /// The correlation ID of the failed request, when known.
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
    },
}

/// A machine-readable error code carried by [`ServerMessage::Error`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// A message was not valid JSON or did not match a known schema.
    Malformed,
    /// A producer set a bus-owned attribute.
    ReservedAttribute,
    /// The request named a subscriber the bus does not know.
    UnknownSubscriber,
    /// The log could not accept the event, for example because the disk is full.
    WriteFailed,
    /// The connection is not permitted to perform the request.
    Forbidden,
}

/// A parsed application message.
#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    /// A control message from a client.
    Client(ClientMessage),
    /// A control message from the bus.
    Server(ServerMessage),
    /// A `CloudEvents` envelope.
    Event(Box<crate::cloudevent::Event>),
}

/// Failures while encoding or decoding a protocol message.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The JSON could not be parsed or serialized.
    #[error("invalid protocol JSON: {0}")]
    Json(#[from] serde_json::Error),
    /// A protocol message was not a JSON object.
    #[error("a protocol message must be a JSON object")]
    NotAnObject,
}

/// Parse an application message from a text frame.
///
/// A top-level object carrying `specversion` is a `CloudEvents` envelope; any
/// other object is a control message, which is parsed as a [`ClientMessage`]
/// first and a [`ServerMessage`] second.
///
/// # Errors
///
/// Returns [`Error::Json`] if `text` is not JSON, is not an object, or matches
/// no known message.
pub fn parse(text: &str) -> Result<Message, Error> {
    let value: Value = serde_json::from_str(text)?;
    let Some(object) = value.as_object() else {
        return Err(Error::NotAnObject);
    };

    if object.contains_key("specversion") {
        let event = serde_json::from_value(value)?;
        return Ok(Message::Event(Box::new(event)));
    }

    // Control messages are tagged by `type`. The client and server schemas have
    // disjoint tags, so trying the client schema first cannot shadow a server
    // message.
    match serde_json::from_value::<ClientMessage>(value.clone()) {
        Ok(message) => Ok(Message::Client(message)),
        Err(_) => Ok(Message::Server(serde_json::from_value(value)?)),
    }
}

/// Serialize a control message to its JSON text.
///
/// # Errors
///
/// Returns [`Error::Json`] if the message cannot be serialized.
pub fn to_text(message: &impl Serialize) -> Result<String, Error> {
    Ok(serde_json::to_string(message)?)
}

#[cfg(test)]
mod tests {
    // Tests for the wire protocol: the classification rule, the documented
    // exchange, and the round-trip of every control message.

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
        let message = parse(r#"{"type":"subscribe","subscriber_id":"audit-log","from_seq":1000}"#)
            .expect("valid");
        assert_eq!(
            message,
            Message::Client(ClientMessage::Subscribe {
                subscriber_id: "audit-log".to_owned(),
                from_seq: 1000,
                filter: None,
            })
        );
    }

    #[test]
    fn a_subscribe_without_a_subscriber_id_is_rejected() {
        // The durable cursor is keyed on the subscriber ID, so it is required.
        assert!(matches!(
            parse(r#"{"type":"subscribe","from_seq":0}"#),
            Err(Error::Json(_))
        ));
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
}
