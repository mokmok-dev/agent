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
mod tests;
