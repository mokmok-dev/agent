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
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Incoming {
    /// Must be `"1.0"`.
    pub specversion: SpecVersion,
    /// The event type, for example `agent.sandbox.egress.requested`.
    #[serde(rename = "type")]
    pub ty: String,
    /// A producer-supplied `source`, which the bus owns and rejects.
    pub source: Option<String>,
    /// A producer-supplied `id`, which the bus owns; a ULID is assigned if
    /// absent.
    pub id: Option<String>,
    /// An optional producer-supplied time, which the bus owns; the commit time
    /// is used if absent.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub time: Option<OffsetDateTime>,
    /// The logical stream, for example a task ID.
    pub subject: Option<String>,
    /// The media type of `data`.
    pub datacontenttype: Option<String>,
    /// A producer-supplied `sequence`, which the bus owns and rejects.
    pub sequence: Option<Sequence>,
    /// The event payload.
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
mod tests;
