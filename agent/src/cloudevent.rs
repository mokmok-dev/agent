//! `CloudEvents` 1.0 envelope model for the event bus.
//!
//! Implements the envelope described in `docs/event-bus/cloudevents.md`: the
//! `CloudEvents` 1.0 JSON format, the bus-owned attributes the bus assigns at
//! commit, and the fixed-width `sequence` encoding the Sequence extension
//! requires.
//!
//! A producer sends an [`Incoming`] event. [`Incoming::commit`] rejects any
//! attempt to set a bus-owned attribute, assigns the missing ones, and returns
//! the canonical [`Event`] that is persisted and delivered.
//!
//! ```
//! use agent::cloudevent::{Incoming, Sequence};
//! use time::OffsetDateTime;
//!
//! let json = br#"{"specversion":"1.0","type":"agent.task.started",
//!     "data":{"task_id":"t-1"}}"#;
//! let incoming: Incoming = serde_json::from_slice(json)?;
//! let now = OffsetDateTime::from_unix_timestamp(0)?;
//! let event = incoming.commit("agent://eventbus", Sequence::new(1024), now)?;
//!
//! assert_eq!(event.source, "agent://eventbus");
//! assert_eq!(event.sequence.get(), 1024);
//! assert_eq!(
//!     serde_json::to_value(&event)?["sequence"],
//!     "00000000000000001024"
//! );
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize, de, ser};
use serde_json::Value;
pub use time::OffsetDateTime;
use ulid::Ulid;

/// Number of characters in the fixed-width decimal `sequence` encoding.
pub const SEQUENCE_WIDTH: usize = 20;

/// The `CloudEvents` specification version this model implements.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SpecVersion {
    /// `CloudEvents` 1.0.
    #[default]
    #[serde(rename = "1.0")]
    V1_0,
}

/// A bus-assigned global sequence number.
///
/// Serialized as a 20-digit zero-padded decimal string so that lexicographic
/// order equals numeric order across the whole `u64` range, as the `CloudEvents`
/// Sequence extension requires. See `docs/event-bus/cloudevents.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Sequence(u64);

impl Sequence {
    /// Wrap a raw sequence number.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// The raw sequence number.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for Sequence {
    fn fmt(
        &self,
        f: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        write!(f, "{:0width$}", self.0, width = SEQUENCE_WIDTH)
    }
}

impl FromStr for Sequence {
    type Err = Error;

    /// # Errors
    ///
    /// Returns [`Error::InvalidSequence`] unless `text` is exactly
    /// [`SEQUENCE_WIDTH`] decimal digits that fit in a `u64`.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let is_canonical =
            text.len() == SEQUENCE_WIDTH && text.bytes().all(|byte| byte.is_ascii_digit());
        if !is_canonical {
            return Err(Error::InvalidSequence(text.to_owned()));
        }
        let value = text
            .parse::<u64>()
            .map_err(|_| Error::InvalidSequence(text.to_owned()))?;
        Ok(Self(value))
    }
}

impl Serialize for Sequence {
    fn serialize<S>(
        &self,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: ser::Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Sequence {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: de::Deserializer<'de>,
    {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(de::Error::custom)
    }
}

/// A committed `CloudEvent`: the envelope persisted in the WAL and delivered to
/// subscribers.
///
/// The bus owns `source`, `id`, `time`, and `sequence` and assigns them at
/// commit; see [`Incoming::commit`].
#[expect(
    clippy::derive_partial_eq_without_eq,
    reason = "serde_json::Value contains f64, so Eq cannot be derived"
)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// Always `"1.0"`.
    pub specversion: SpecVersion,
    /// The event type, for example `agent.task.started`.
    #[serde(rename = "type")]
    pub ty: String,
    /// The bus instance URI.
    pub source: String,
    /// A unique identifier, a ULID when the bus assigned it.
    pub id: String,
    /// The RFC 3339 commit time.
    #[serde(
        default,
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub time: Option<OffsetDateTime>,
    /// The logical stream, for example a task ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    /// The media type of `data`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub datacontenttype: Option<String>,
    /// The bus-assigned global sequence number.
    pub sequence: Sequence,
    /// The event payload.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    /// Additional extension attributes, keyed by name.
    #[serde(flatten)]
    pub extensions: BTreeMap<String, Value>,
}

impl Event {
    /// Serialize to the canonical `CloudEvents` JSON bytes stored in the WAL.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Json`] if the event cannot be serialized.
    pub fn to_bytes(&self) -> Result<Vec<u8>, Error> {
        Ok(serde_json::to_vec(self)?)
    }

    /// Parse a canonical event from its `CloudEvents` JSON bytes.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Json`] if the bytes are not a valid `CloudEvents` 1.0
    /// event.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        Ok(serde_json::from_slice(bytes)?)
    }
}

/// A producer-supplied event before the bus assigns its bus-owned attributes.
#[expect(
    clippy::derive_partial_eq_without_eq,
    reason = "serde_json::Value contains f64, so Eq cannot be derived"
)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Incoming {
    /// Must be `"1.0"`.
    pub specversion: SpecVersion,
    /// The event type, for example `agent.sandbox.egress.requested`.
    #[serde(rename = "type")]
    pub ty: String,
    /// A producer-supplied `source`, which the bus owns and rejects.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// A producer-supplied `id`, which the bus owns; a ULID is assigned if
    /// absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// An optional producer-supplied time, which the bus owns; the commit time
    /// is used if absent.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub time: Option<OffsetDateTime>,
    /// The logical stream, for example a task ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    /// The media type of `data`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub datacontenttype: Option<String>,
    /// A producer-supplied `sequence`, which the bus owns and rejects.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sequence: Option<Sequence>,
    /// The event payload.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    /// Additional extension attributes, keyed by name.
    #[serde(flatten)]
    pub extensions: BTreeMap<String, Value>,
}

impl Incoming {
    /// Assign the bus-owned attributes and produce the canonical event.
    ///
    /// Assigns `source`, `sequence`, and `time`, and assigns a ULID `id` when
    /// the producer omitted one.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ReservedAttribute`] if the producer set `source` or
    /// `sequence`, which the bus owns.
    pub fn commit(
        self,
        source: &str,
        sequence: Sequence,
        now: OffsetDateTime,
    ) -> Result<Event, Error> {
        if self.source.is_some() {
            return Err(Error::ReservedAttribute("source"));
        }
        if self.sequence.is_some() {
            return Err(Error::ReservedAttribute("sequence"));
        }
        Ok(Event {
            specversion: self.specversion,
            ty: self.ty,
            source: source.to_owned(),
            id: self.id.unwrap_or_else(|| Ulid::new().to_string()),
            time: Some(self.time.unwrap_or(now)),
            subject: self.subject,
            datacontenttype: self.datacontenttype,
            sequence,
            data: self.data,
            extensions: self.extensions,
        })
    }
}

/// Failures of `CloudEvents` parsing and bus assignment.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A producer set an attribute the bus owns.
    #[error("a producer must not set the bus-owned attribute `{0}`")]
    ReservedAttribute(&'static str),
    /// A `sequence` was not a 20-digit decimal string.
    #[error("sequence `{0}` is not a {SEQUENCE_WIDTH}-digit decimal string")]
    InvalidSequence(String),
    /// The event JSON could not be parsed or serialized.
    #[error("invalid CloudEvent JSON: {0}")]
    Json(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    // Unit tests for the `CloudEvents` envelope: the worked examples from the design
    // docs, the bus-owned attribute rules, and the fixed-width `sequence` encoding.

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
    fn commit_preserves_a_producer_traceparent() {
        // The bus owns only `source`, `id`, `time`, and `sequence`. A W3C
        // `traceparent` is the producer's, so the bus must carry it through
        // unchanged: the audit log and the operational trace then share one
        // trace id without the bus running an OpenTelemetry SDK.
        let json = br#"{"specversion":"1.0","type":"agent.sandbox.egress.requested",
            "traceparent":"00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"}"#;
        let incoming: Incoming = serde_json::from_slice(json).expect("a valid incoming event");
        let traceparent = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

        let event = incoming
            .commit("agent://eventbus", Sequence::new(1), epoch())
            .expect("no reserved attribute was set");

        assert_eq!(
            event.extensions.get("traceparent"),
            Some(&json!(traceparent)),
            "the bus must not rewrite a producer's trace context",
        );
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
}
