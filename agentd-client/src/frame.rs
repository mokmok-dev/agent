//! The frames the client server writes on the downstream socket.
//!
//! A committed event is forwarded as the daemon's own [`WireEnvelope`] shape, so
//! a client parses one envelope whether it came from the daemon directly or
//! through this server. The notices below are local to the downstream socket:
//! they are never published to the daemon and never reach the log.

use agentd_events::{Event, WireEnvelope};
use axum::extract::ws::Message;
use serde_json::{Value, json};

/// The `CloudEvents` source stamped on a local notice.
pub const CLIENT_SOURCE: &str = "urn:mokmokd:client";

/// The notice reporting a committed publish, correlated by `event_id`.
pub const PUBLISH_COMMITTED: &str = "client.publish_committed";

/// The notice reporting a refused, rejected, or unresolved publish.
pub const PUBLISH_FAILED: &str = "client.publish_failed";

/// The notice sent just before the downstream socket closes because the daemon
/// connection ended.
pub const UPSTREAM_LOST: &str = "client.upstream_lost";

/// Renders a committed event, or a daemon notice, as its downstream frame.
///
/// The shape is the daemon's [`WireEnvelope`], including `"seq": null` for a
/// transient notice. Rendering is infallible: the payload is already JSON, so it
/// goes through [`Value::to_string`] rather than a fallible serializer.
#[must_use]
pub fn wire(envelope: &WireEnvelope) -> Message {
    Message::text(json!({ "seq": envelope.seq, "event": &envelope.event }).to_string())
}

/// Builds a downstream-only notice frame.
///
/// `seq` is `null`, so a client that keeps a checkpoint must not advance it on a
/// notice — the rule the daemon's own notices already require.
#[must_use]
pub fn notice(
    r#type: &str,
    data: Value,
) -> Message {
    let mut event = Event::new(r#type, data);
    event.source = String::from(CLIENT_SOURCE);
    Message::text(json!({ "seq": Value::Null, "event": event }).to_string())
}
