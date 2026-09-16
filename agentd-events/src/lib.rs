//! The event contract and transport at the core of the choreography-style
//! event flow: components publish events and react to the events published by
//! others; there is no central orchestrator.
//!
//! Events conform to [CloudEvents] 1.0: every event carries the required
//! context attributes (`id`, `source`, `specversion`, `type`) serialized with
//! their spec-defined names, plus the event `data`.
//!
//! An internal live fanout delivers each committed event to subscribers;
//! [`EventLog`] is the durable append-only JSONL log that is the source of
//! truth: it writes an event before fanning it out, so an acknowledged event
//! cannot be lost. [`projection`] holds read models replayed from the log.
//!
//! Everything that leaves the log — the live fanout and the WebSocket API —
//! carries the event's [`Seq`] alongside it: as a [`LogEntry`] in-process, and
//! as a [`WireMessage`] on the wire, so a consumer can record how far it has
//! processed and resume from there.
//!
//! [CloudEvents]: https://github.com/cloudevents/spec/blob/v1.0.2/cloudevents/spec.md

use serde::{Deserialize, Serialize};
use thiserror::Error as ThisError;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::sync::broadcast;

pub mod chain;
pub mod log;
pub mod projection;

pub use log::{EventLog, LogError, LogReader, Seq, verify_chain};
pub use projection::{Projection, ProjectionError, catch_up};

/// Capacity of the channel buffering events per subscriber before it lags.
const EVENT_CHANNEL_CAPACITY: usize = 1024;

/// The `CloudEvents` specification version all events conform to.
pub const SPEC_VERSION: &str = "1.0";

/// The `source` attribute assigned to events produced by the daemon itself.
pub const DAEMON_SOURCE: &str = "urn:mokmokd";

/// `type` prefixes that only a daemon-authority publisher may emit.
///
/// These carry decisions or daemon lifecycle that a client must not be able to
/// fabricate — above all `sandbox.permission.granted`/`denied`, which drive the
/// approval flow, and `error.*`, which is a daemon notice. An external
/// extension is expected to use its own prefix; see `docs/architecture.md` and
/// `docs/sandbox.md`.
pub const RESERVED_TYPE_PREFIXES: &[&str] = &["error.", "sandbox.", "session."];

/// Whether an event `type` is reserved to daemon-authority publishers.
#[must_use]
pub fn is_reserved_type(r#type: &str) -> bool {
    RESERVED_TYPE_PREFIXES
        .iter()
        .any(|prefix| r#type.starts_with(prefix))
}

/// Why an event is not acceptable on ingress.
///
/// A client-supplied event is validated before it is appended, so a malformed
/// or unauthenticated event never becomes part of the durable log.
#[derive(Debug, Clone, PartialEq, Eq, ThisError)]
pub enum InvalidEvent {
    /// The `specversion` is not the supported `CloudEvents` version.
    #[error("specversion must be {SPEC_VERSION}")]
    SpecVersion,
    /// The `type` attribute is empty or contains whitespace.
    #[error("type must be a non-empty dotted name")]
    Type,
    /// The `id` attribute is not a UUID.
    #[error("id must be a UUID")]
    Id,
    /// The `time` attribute is present but is not an RFC 3339 timestamp.
    #[error("time must be an RFC 3339 timestamp")]
    Time,
}

/// An event exchanged between agentd components and connected clients.
///
/// The serialized form uses the `CloudEvents` 1.0 attribute names; the `type`
/// attribute is exposed as the `r#type` field of [`Event`], the raw-identifier
/// spelling of the Rust keyword.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    /// The `CloudEvents` `id` attribute, generated as a UUID version 7 so
    /// that events order by creation time.
    pub id: String,
    /// The `CloudEvents` `source` attribute identifying where the event
    /// originated.
    pub source: String,
    /// The `CloudEvents` `specversion` attribute.
    pub specversion: String,
    /// The `CloudEvents` `type` attribute.
    pub r#type: String,
    /// The `CloudEvents` `time` attribute as an RFC 3339 timestamp. Optional in
    /// the specification, so absent timestamps are tolerated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time: Option<String>,
    /// The `CloudEvents` `data` payload.
    #[serde(default)]
    pub data: serde_json::Value,
}

impl Event {
    /// Creates a daemon-produced event with `type` and `data`, assigning a
    /// fresh `id`, the daemon `source`, the current `specversion`, and the
    /// current UTC `time`.
    ///
    /// The `time` attribute is left unset if the current time cannot be
    /// formatted as RFC 3339 (e.g. a system clock far outside the representable
    /// range); `time` is optional in the specification.
    pub fn new(
        r#type: impl Into<String>,
        data: serde_json::Value,
    ) -> Self {
        Self {
            id: uuid::Uuid::now_v7().to_string(),
            source: String::from(DAEMON_SOURCE),
            specversion: String::from(SPEC_VERSION),
            r#type: r#type.into(),
            time: OffsetDateTime::now_utc().format(&Rfc3339).ok(),
            data,
        }
    }

    /// Whether this event's `type` is reserved to daemon-authority publishers
    /// (see [`is_reserved_type`]).
    #[must_use]
    pub fn is_reserved(&self) -> bool {
        is_reserved_type(&self.r#type)
    }

    /// Overwrites the provenance attributes the daemon owns on ingress: the
    /// `source` is replaced with the authenticated principal's, and `time` with
    /// the daemon's current UTC time, so a client cannot forge where or when an
    /// event entered the log.
    pub fn set_provenance(
        &mut self,
        source: impl Into<String>,
    ) {
        self.source = source.into();
        self.time = OffsetDateTime::now_utc().format(&Rfc3339).ok();
    }

    /// Validates the `CloudEvents` context attributes accepted on ingress.
    ///
    /// The daemon owns `source` and `time` and overwrites them, so this checks
    /// only what a client is trusted to supply: the `specversion`, a non-empty
    /// `type`, a UUID `id`, and an RFC 3339 `time` when present.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidEvent`] naming the offending attribute.
    pub fn validate(&self) -> Result<(), InvalidEvent> {
        if self.specversion != SPEC_VERSION {
            return Err(InvalidEvent::SpecVersion);
        }
        if self.r#type.trim().is_empty() || self.r#type.chars().any(char::is_whitespace) {
            return Err(InvalidEvent::Type);
        }
        if uuid::Uuid::parse_str(&self.id).is_err() {
            return Err(InvalidEvent::Id);
        }
        if let Some(time) = &self.time
            && OffsetDateTime::parse(time, &Rfc3339).is_err()
        {
            return Err(InvalidEvent::Time);
        }
        Ok(())
    }
}

/// An [`Event`] together with its position in the log.
///
/// This is the unit that flows on the log's live fanout (see
/// [`EventLog::subscribe`]). Because the position travels with the event, a
/// consumer can persist the last [`seq`](LogEntry::seq) it applied and resume
/// from there with [`EventLog::read_from`].
///
/// The position is in-process metadata; the shape sent over the WebSocket API
/// is [`WireMessage`], whose `seq` is optional to admit transient daemon
/// notices that are not part of the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEntry {
    /// The one-based log position of [`event`](LogEntry::event).
    pub seq: Seq,
    /// The committed event.
    pub event: Event,
}

impl LogEntry {
    /// Pairs `event` with its log position `seq`.
    #[must_use]
    pub const fn new(
        seq: Seq,
        event: Event,
    ) -> Self {
        Self { seq, event }
    }
}

/// The JSON envelope exchanged with WebSocket clients: an [`Event`] and its log
/// position.
///
/// `seq` is the one-based log position, or `None` for a transient daemon notice
/// that is not part of the log (for example `error.lagged`). Inbound frames are
/// a bare [`Event`]; the daemon assigns the position on append, so `seq` is
/// produced by the daemon and never by a client.
///
/// [`Event`] serializes as its verbatim `CloudEvents` envelope, so a
/// `WireMessage` is `{ "seq": <n|null>, "event": <cloud event> }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireMessage {
    /// The one-based log position, or `None` for a transient notice.
    #[serde(default)]
    pub seq: Option<Seq>,
    /// The event, as a verbatim `CloudEvents` envelope.
    pub event: Event,
}

impl WireMessage {
    /// Wraps a transient daemon notice that has no log position.
    #[must_use]
    pub const fn notice(event: Event) -> Self {
        Self { seq: None, event }
    }
}

impl From<LogEntry> for WireMessage {
    /// Wraps a committed log entry with its position.
    fn from(entry: LogEntry) -> Self {
        Self {
            seq: Some(entry.seq),
            event: entry.event,
        }
    }
}

/// Broadcasts [`LogEntry`] events to every subscriber.
///
/// This is an internal detail of [`EventLog`], which is the only publisher:
/// every `LogEntry` it sends carries the position the event actually received
/// in the log. Use [`EventLog::subscribe`] to receive the fanout.
#[derive(Debug, Clone)]
pub(crate) struct EventBus {
    sender: broadcast::Sender<LogEntry>,
}

impl EventBus {
    /// Creates a bus whose channel buffers up to `capacity` events per
    /// subscriber before the subscriber lags.
    ///
    /// # Panics
    ///
    /// Panics if `capacity` is zero.
    #[must_use]
    pub(crate) fn new(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity);
        Self { sender }
    }

    /// Publishes `recorded` to all current subscribers.
    ///
    /// Publishing without subscribers is not an error; the event is dropped
    /// and the condition is logged.
    pub(crate) fn publish(
        &self,
        recorded: LogEntry,
    ) {
        if self.sender.send(recorded).is_err() {
            tracing::debug!("event dropped because there are no subscribers");
        }
    }

    /// Subscribes to all events published after this call.
    #[must_use]
    pub(crate) fn subscribe(&self) -> broadcast::Receiver<LogEntry> {
        self.sender.subscribe()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new(EVENT_CHANNEL_CAPACITY)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DAEMON_SOURCE, Event, EventBus, InvalidEvent, LogEntry, SPEC_VERSION, is_reserved_type,
    };
    use serde_json::json;
    use tokio::sync::broadcast::error::RecvError;

    fn test_event(r#type: &str) -> Event {
        Event::new(r#type, json!({ "value": 1 }))
    }

    #[test]
    fn new_assigns_cloud_events_attributes() {
        let event = test_event("test.event");

        assert_eq!(event.source, DAEMON_SOURCE);
        assert_eq!(event.specversion, SPEC_VERSION);
        assert_eq!(event.r#type, "test.event");
        uuid::Uuid::parse_str(&event.id).expect("id should be a UUID");
        let time = event.time.expect("time should be set");
        time::OffsetDateTime::parse(&time, &time::format_description::well_known::Rfc3339)
            .expect("time should be an RFC 3339 timestamp");
    }

    #[test]
    fn deserializes_minimal_cloud_event_with_optional_attributes_absent() {
        let raw = r#"{
            "id": "018f6b2e-7e5c-7000-8000-000000000000",
            "source": "urn:mokmokd",
            "specversion": "1.0",
            "type": "test.event"
        }"#;

        let event: Event = serde_json::from_str(raw).expect("should deserialize");

        assert_eq!(event.time, None);
        assert_eq!(event.data, serde_json::Value::Null);
    }

    #[tokio::test]
    async fn publish_delivers_events_to_all_subscribers() {
        let bus = EventBus::new(8);
        let mut first = bus.subscribe();
        let mut second = bus.subscribe();
        let recorded = LogEntry::new(1, test_event("test.event"));

        bus.publish(recorded.clone());

        assert_eq!(first.recv().await.ok().as_ref(), Some(&recorded));
        assert_eq!(second.recv().await.ok().as_ref(), Some(&recorded));
    }

    #[tokio::test]
    async fn publish_without_subscribers_is_not_an_error() {
        let bus = EventBus::new(8);

        bus.publish(LogEntry::new(1, test_event("test.event")));
    }

    #[tokio::test]
    async fn lagged_subscribers_observe_lagged_before_events_resume() {
        let bus = EventBus::new(1);
        let mut subscriber = bus.subscribe();
        let second = LogEntry::new(2, test_event("test.second"));
        bus.publish(LogEntry::new(1, test_event("test.first")));
        bus.publish(second.clone());

        let received = subscriber.recv().await;

        assert!(matches!(received, Err(RecvError::Lagged(1))));
        assert_eq!(subscriber.recv().await.ok().as_ref(), Some(&second));
    }

    #[test]
    fn reserved_types_cover_authority_event_families() {
        assert!(is_reserved_type("sandbox.permission.granted"));
        assert!(is_reserved_type("sandbox.exec.completed"));
        assert!(is_reserved_type("error.invalid_event"));
        assert!(is_reserved_type("session.started"));
        assert!(!is_reserved_type("task.submitted"));
        assert!(!is_reserved_type("error"));
    }

    #[test]
    fn validate_rejects_malformed_context_attributes() {
        let mut event = test_event("test.event");
        assert_eq!(event.validate(), Ok(()));

        event.specversion = String::from("0.3");
        assert_eq!(event.validate(), Err(InvalidEvent::SpecVersion));

        let mut event = test_event("test.event");
        event.r#type = String::from("  ");
        assert_eq!(event.validate(), Err(InvalidEvent::Type));
        let mut event = test_event("test.event");
        event.r#type = String::from("bad type");
        assert_eq!(event.validate(), Err(InvalidEvent::Type));

        let mut event = test_event("test.event");
        event.id = String::from("not-a-uuid");
        assert_eq!(event.validate(), Err(InvalidEvent::Id));

        let mut event = test_event("test.event");
        event.time = Some(String::from("yesterday"));
        assert_eq!(event.validate(), Err(InvalidEvent::Time));
        let mut event = test_event("test.event");
        event.time = None;
        assert_eq!(event.validate(), Ok(()));
    }
}
