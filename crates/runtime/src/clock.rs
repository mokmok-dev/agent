//! Reading the wall clock.
//!
//! The core deals in [`Timestamp`] and never reads a clock, so the one place
//! that asks the operating system for the time is here.

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use agent_core::time::Timestamp;

/// Why the clock could not be read.
#[derive(Debug)]
pub enum ClockError {
    /// The system clock is set before the Unix epoch.
    BeforeEpoch,
    /// The system clock does not fit the range a timestamp can hold.
    OutOfRange,
}

impl fmt::Display for ClockError {
    fn fmt(
        &self,
        formatter: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        match self {
            Self::BeforeEpoch => write!(formatter, "the system clock is before the Unix epoch"),
            Self::OutOfRange => write!(formatter, "the system clock is out of range"),
        }
    }
}

impl std::error::Error for ClockError {}

/// The current time, at second and nanosecond resolution.
///
/// # Errors
///
/// Returns [`ClockError`] when the system clock is before the epoch or holds a
/// value a [`Timestamp`] cannot represent.
pub fn now() -> Result<Timestamp, ClockError> {
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| ClockError::BeforeEpoch)?;
    let secs = i64::try_from(since.as_secs()).map_err(|_| ClockError::OutOfRange)?;
    Timestamp::new(secs, since.subsec_nanos()).map_err(|_| ClockError::OutOfRange)
}

#[cfg(test)]
mod tests {
    use super::now;

    #[test]
    fn the_clock_is_after_the_year_two_thousand() {
        let now = now().expect("reads the clock");
        assert!(now.secs() > 946_684_800, "{now:?}");
    }

    #[test]
    fn the_clock_does_not_go_backwards_between_two_reads() {
        let first = now().expect("reads the clock");
        let second = now().expect("reads the clock");
        assert!(second.secs() >= first.secs());
    }

    #[test]
    fn every_error_explains_itself() {
        assert_ne!(super::ClockError::BeforeEpoch.to_string(), "");
        assert_ne!(super::ClockError::OutOfRange.to_string(), "");
    }
}
