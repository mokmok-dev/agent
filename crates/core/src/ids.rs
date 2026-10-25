//! Identifiers. Each one is a newtype over fixed bytes, so a branch id cannot
//! be passed where an event id is expected, and the text form exists only at
//! the boundary.

use alloc::string::String;
use core::fmt;
use core::str::FromStr;

/// Identifier errors, collapsed into the few cases a caller can act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdError {
    /// The text was the wrong length for this identifier.
    BadLength,
    /// A character was not a lowercase or uppercase hex digit.
    BadDigit,
    /// A prefixed identifier did not start with its prefix.
    BadPrefix,
    /// An actor string did not start with `human:`, `process:`, or `agent:`.
    BadActor,
}

impl fmt::Display for IdError {
    fn fmt(
        &self,
        formatter: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        let text = match self {
            Self::BadLength => "wrong length",
            Self::BadDigit => "not a hex digit",
            Self::BadPrefix => "wrong prefix",
            Self::BadActor => "actor must start with human:, process:, or agent:",
        };
        formatter.write_str(text)
    }
}

impl core::error::Error for IdError {}

const HEX: &[u8; 16] = b"0123456789abcdef";

fn write_hex(
    bytes: &[u8],
    out: &mut impl fmt::Write,
) -> fmt::Result {
    for byte in bytes {
        let high = HEX[usize::from(byte >> 4)];
        let low = HEX[usize::from(byte & 0x0f)];
        out.write_char(char::from(high))?;
        out.write_char(char::from(low))?;
    }
    Ok(())
}

const fn nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn read_hex<const N: usize>(text: &str) -> Result<[u8; N], IdError> {
    let bytes = text.as_bytes();
    if bytes.len() != N * 2 {
        return Err(IdError::BadLength);
    }
    let mut out = [0_u8; N];
    for (index, slot) in out.iter_mut().enumerate() {
        let high = nibble(bytes[index * 2]).ok_or(IdError::BadDigit)?;
        let low = nibble(bytes[index * 2 + 1]).ok_or(IdError::BadDigit)?;
        *slot = high * 16 + low;
    }
    Ok(out)
}

/// A content hash. The same bytes always produce the same id, and the id is
/// the proof that the bytes were not altered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EventId([u8; 32]);

impl EventId {
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for EventId {
    fn fmt(
        &self,
        formatter: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        write_hex(&self.0, formatter)
    }
}

impl FromStr for EventId {
    type Err = IdError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        read_hex(text).map(Self)
    }
}

/// A branch, named by a short random id that never changes. The label a person
/// reads is separate, so a rename cannot break a reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BranchId([u8; 8]);

impl BranchId {
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 8]) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 8] {
        &self.0
    }
}

impl fmt::Display for BranchId {
    fn fmt(
        &self,
        formatter: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        formatter.write_str("b-")?;
        write_hex(&self.0, formatter)
    }
}

impl FromStr for BranchId {
    type Err = IdError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let rest = text.strip_prefix("b-").ok_or(IdError::BadPrefix)?;
        read_hex(rest).map(Self)
    }
}

/// One conversation turn. A turn is the unit of retry and of cache accounting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TurnId([u8; 8]);

impl TurnId {
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 8]) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 8] {
        &self.0
    }
}

impl fmt::Display for TurnId {
    fn fmt(
        &self,
        formatter: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        write_hex(&self.0, formatter)
    }
}

impl FromStr for TurnId {
    type Err = IdError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        read_hex(text).map(Self)
    }
}

/// One tool invocation inside a turn. The step index is what makes the id
/// reproducible when the same turn is folded twice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CallId {
    pub turn: TurnId,
    pub step: u32,
}

impl fmt::Display for CallId {
    fn fmt(
        &self,
        formatter: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        write!(formatter, "{}:{}", self.turn, self.step)
    }
}

/// Who caused an event. The prefix is part of the text form because a
/// consumer classifies trust by it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Actor {
    Human(String),
    Process(String),
    Agent(String),
}

impl Actor {
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::Human(name) | Self::Process(name) | Self::Agent(name) => name,
        }
    }

    #[must_use]
    pub const fn prefix(&self) -> &'static str {
        match self {
            Self::Human(_) => "human",
            Self::Process(_) => "process",
            Self::Agent(_) => "agent",
        }
    }
}

impl fmt::Display for Actor {
    fn fmt(
        &self,
        formatter: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        write!(formatter, "{}:{}", self.prefix(), self.name())
    }
}

impl FromStr for Actor {
    type Err = IdError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let (prefix, name) = text.split_once(':').ok_or(IdError::BadActor)?;
        if name.is_empty() {
            return Err(IdError::BadActor);
        }
        let actor = match prefix {
            "human" => Self::Human,
            "process" => Self::Process,
            "agent" => Self::Agent,
            _ => return Err(IdError::BadActor),
        };
        Ok(actor(String::from(name)))
    }
}

#[cfg(test)]
mod tests {
    use super::{Actor, BranchId, CallId, EventId, IdError, TurnId};
    use alloc::string::ToString;
    use core::str::FromStr;

    #[test]
    fn an_event_id_round_trips_through_hex() {
        let id = EventId::from_bytes([0xab; 32]);
        let text = id.to_string();
        assert_eq!(text.len(), 64);
        assert!(text.starts_with("abab"));
        assert_eq!(EventId::from_str(&text), Ok(id));
    }

    #[test]
    fn uppercase_hex_is_accepted() {
        let text = "AB".repeat(32);
        assert_eq!(
            EventId::from_str(&text),
            Ok(EventId::from_bytes([0xab; 32]))
        );
    }

    #[test]
    fn a_short_id_is_a_length_error() {
        assert_eq!(EventId::from_str("ab"), Err(IdError::BadLength));
    }

    #[test]
    fn a_non_hex_character_is_a_digit_error() {
        let mut text = "ab".repeat(32);
        text.replace_range(0..1, "z");
        assert_eq!(EventId::from_str(&text), Err(IdError::BadDigit));
    }

    #[test]
    fn a_branch_id_carries_its_prefix() {
        let branch = BranchId::from_bytes([0x0f; 8]);
        assert_eq!(branch.to_string(), "b-0f0f0f0f0f0f0f0f");
        assert_eq!(BranchId::from_str(&branch.to_string()), Ok(branch));
    }

    #[test]
    fn a_branch_id_without_the_prefix_is_rejected() {
        assert_eq!(BranchId::from_str("0f0f"), Err(IdError::BadPrefix));
    }

    #[test]
    fn a_turn_id_round_trips() {
        let turn = TurnId::from_bytes([1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(TurnId::from_str(&turn.to_string()), Ok(turn));
    }

    #[test]
    fn a_call_id_names_its_turn_and_step() {
        let call = CallId {
            turn: TurnId::from_bytes([0; 8]),
            step: 7,
        };
        assert_eq!(call.to_string(), "0000000000000000:7");
    }

    #[test]
    fn actors_round_trip_with_their_prefix() {
        let actors = [
            Actor::Human("ana".to_string()),
            Actor::Process("nightly".to_string()),
            Actor::Agent("deepseek".to_string()),
        ];
        let texts = ["human:ana", "process:nightly", "agent:deepseek"];
        for (actor, text) in actors.iter().zip(texts) {
            assert_eq!(actor.to_string(), text);
            assert_eq!(Actor::from_str(text).as_ref(), Ok(actor));
        }
    }

    #[test]
    fn an_actor_without_a_known_prefix_is_rejected() {
        assert_eq!(Actor::from_str("robot:x"), Err(IdError::BadActor));
        assert_eq!(Actor::from_str("nocolon"), Err(IdError::BadActor));
        assert_eq!(Actor::from_str("human:"), Err(IdError::BadActor));
    }

    #[test]
    fn the_error_text_says_what_went_wrong() {
        let messages = [
            (IdError::BadLength, "wrong length"),
            (IdError::BadDigit, "not a hex digit"),
            (IdError::BadPrefix, "wrong prefix"),
            (
                IdError::BadActor,
                "actor must start with human:, process:, or agent:",
            ),
        ];
        for (error, expected) in messages {
            assert_eq!(error.to_string(), expected);
        }
    }
}
