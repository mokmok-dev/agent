//! W3C Trace Context `traceparent` support.
//!
//! A `traceparent` is a `CloudEvents` extension attribute (lowercase
//! alphanumeric, as the specification requires of extension names) carrying the
//! W3C Trace Context header value described by
//! [Trace Context](https://www.w3.org/TR/trace-context/#traceparent-header):
//!
//! ```text
//! version-trace-id-parent-id-trace-flags
//! 00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01
//! ```
//!
//! The daemon accepts a `traceparent` from a client, so it is validated before
//! it enters the durable log: a malformed value is rejected rather than stored.

use thiserror::Error as ThisError;

/// The `CloudEvents` extension attribute carrying the W3C `traceparent`.
pub const TRACEPARENT_ATTR: &str = "traceparent";

/// The version this parser understands.
const VERSION: &str = "00";

/// A parsed W3C Trace Context `traceparent`.
///
/// Only version `00` is accepted: the daemon and its nodes all speak it, and a
/// newer version's leading fields cannot be assumed to have the same layout, so
/// an unknown version is rejected rather than parsed speculatively.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Traceparent {
    /// The 16-byte trace id, as 32 lowercase hex digits.
    trace_id: String,
    /// The 8-byte parent (span) id, as 16 lowercase hex digits.
    span_id: String,
    /// The trace flags octet.
    flags: u8,
}

/// Why a `traceparent` is not acceptable.
#[derive(Debug, Clone, PartialEq, Eq, ThisError)]
pub enum TraceparentError {
    /// The value is not four dash-separated fields.
    #[error("traceparent must be version-trace-id-parent-id-flags")]
    Shape,
    /// The version is not the supported `00`.
    #[error("traceparent version must be 00")]
    Version,
    /// A field has the wrong length or contains a non-hex character.
    #[error("traceparent fields must be hex of the documented length")]
    Field,
    /// The trace id or parent id is all zeroes, which the specification forbids.
    #[error("traceparent trace id and parent id must not be zero")]
    Zero,
}

impl Traceparent {
    /// Parses and validates a `traceparent` value.
    ///
    /// # Errors
    ///
    /// Returns [`TraceparentError`] naming the violated rule.
    pub fn parse(value: &str) -> Result<Self, TraceparentError> {
        let mut fields = value.split('-');
        let (Some(version), Some(trace_id), Some(span_id), Some(flags), None) = (
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
        ) else {
            return Err(TraceparentError::Shape);
        };
        if version != VERSION {
            return Err(TraceparentError::Version);
        }
        if !is_hex(trace_id, 32) || !is_hex(span_id, 16) || !is_hex(flags, 2) {
            return Err(TraceparentError::Field);
        }
        if is_zero(trace_id) || is_zero(span_id) {
            return Err(TraceparentError::Zero);
        }
        let flags = u8::from_str_radix(flags, 16).map_err(|_| TraceparentError::Field)?;
        Ok(Self {
            trace_id: trace_id.to_ascii_lowercase(),
            span_id: span_id.to_ascii_lowercase(),
            // W3C requires unknown flag bits to be zeroed, so only the sampled
            // bit is kept.
            flags: flags & 0x01,
        })
    }

    /// The trace id, as 32 lowercase hex digits.
    #[must_use]
    pub fn trace_id(&self) -> &str {
        &self.trace_id
    }

    /// The parent (span) id, as 16 lowercase hex digits.
    #[must_use]
    pub fn span_id(&self) -> &str {
        &self.span_id
    }

    /// The trace flags octet.
    ///
    /// Only the `sampled` bit is meaningful per W3C; unknown bits are zeroed on
    /// parse.
    #[must_use]
    pub const fn flags(&self) -> u8 {
        self.flags
    }

    /// Whether the sampled flag (the least significant bit of the flags octet)
    /// is set.
    #[must_use]
    pub const fn sampled(&self) -> bool {
        self.flags & 0x01 == 0x01
    }

    /// Renders the value in the W3C header format.
    #[must_use]
    pub fn to_header(&self) -> String {
        format!(
            "{VERSION}-{}-{}-{:02x}",
            self.trace_id, self.span_id, self.flags
        )
    }
}

/// Whether `value` is exactly `length` hexadecimal characters.
fn is_hex(
    value: &str,
    length: usize,
) -> bool {
    value.len() == length && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Whether every hexadecimal digit in `value` is `0`.
fn is_zero(value: &str) -> bool {
    value.bytes().all(|byte| byte == b'0')
}

impl std::fmt::Display for Traceparent {
    fn fmt(
        &self,
        formatter: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        formatter.write_str(&self.to_header())
    }
}

impl std::str::FromStr for Traceparent {
    type Err = TraceparentError;

    /// Parses a `traceparent` value; see [`Traceparent::parse`].
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

#[cfg(test)]
mod tests {
    use super::{Traceparent, TraceparentError};

    const VALID: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

    #[test]
    fn parses_a_valid_traceparent() {
        let parsed = Traceparent::parse(VALID).expect("valid");

        assert_eq!(parsed.trace_id(), "4bf92f3577b34da6a3ce929d0e0e4736");
        assert_eq!(parsed.span_id(), "00f067aa0ba902b7");
        assert!(parsed.sampled());
        assert_eq!(parsed.to_header(), VALID);
    }

    #[test]
    fn unknown_flag_bits_are_zeroed() {
        // `ff` has every bit set; only the sampled bit survives.
        let parsed = Traceparent::parse("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-ff")
            .expect("valid");

        assert_eq!(parsed.flags(), 0x01);
        assert_eq!(
            parsed.to_header(),
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
        );
    }

    #[test]
    fn unsampled_flag_is_reported() {
        let parsed = Traceparent::parse("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-00")
            .expect("valid");

        assert!(!parsed.sampled());
    }

    #[test]
    fn rejects_malformed_values() {
        assert_eq!(Traceparent::parse("nope"), Err(TraceparentError::Shape));
        assert_eq!(
            Traceparent::parse("01-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"),
            Err(TraceparentError::Version)
        );
        assert_eq!(
            Traceparent::parse("00-xyz-00f067aa0ba902b7-01"),
            Err(TraceparentError::Field)
        );
        assert_eq!(
            Traceparent::parse("00-00000000000000000000000000000000-00f067aa0ba902b7-01"),
            Err(TraceparentError::Zero)
        );
        assert_eq!(
            Traceparent::parse("00-4bf92f3577b34da6a3ce929d0e0e4736-0000000000000000-01"),
            Err(TraceparentError::Zero)
        );
    }

    #[test]
    fn accepts_uppercase_hex_and_normalises_it() {
        let parsed = Traceparent::parse("00-4BF92F3577B34DA6A3CE929D0E0E4736-00F067AA0BA902B7-01")
            .expect("valid");

        assert_eq!(parsed.to_header(), VALID);
    }
}
