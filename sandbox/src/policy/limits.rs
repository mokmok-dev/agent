//! The limits domain: a wall-clock timeout and an output cap.
//!
//! There is no memory field. macOS has no enforcement mechanism and Linux is not
//! given a cgroup, so a memory limit would be a promise the kernel does not
//! keep; see `docs/sandbox/security.md`.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::InvalidPolicy;

/// Default wall-clock timeout, in milliseconds: five minutes.
pub const DEFAULT_TIMEOUT_MILLIS: u64 = 300_000;

/// Default output cap: one mebibyte.
pub const DEFAULT_MAX_OUTPUT_BYTES: u64 = 1024 * 1024;

/// The wall-clock timeout and output cap for a command.
///
/// The timeout is held as **milliseconds**, not a [`Duration`], so the policy
/// has a plain serde wire form: `Duration` has no serde impl and no crate here
/// provides one. [`Limits::timeout`] returns the [`Duration`] for callers that
/// want it.
///
/// The [`Default`] is **non-zero on purpose**. A derived zero would kill every
/// command the instant it started and let it produce no output at all, so a
/// policy that forgot the domain would fail in a way that looks like the
/// command's fault. A non-zero default means a policy is never rejected for a
/// zeroed limit, and an operator who wants a different bound sets it explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    /// Milliseconds of wall-clock time before the process group is killed.
    pub timeout_millis: u64,
    /// Maximum bytes of combined output retained.
    pub max_output_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            timeout_millis: DEFAULT_TIMEOUT_MILLIS,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
        }
    }
}

impl Limits {
    /// The wall-clock timeout as a [`Duration`].
    #[must_use]
    pub const fn timeout(self) -> Duration {
        Duration::from_millis(self.timeout_millis)
    }

    /// Check that both limits are non-zero.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidPolicy::ZeroTimeout`] or
    /// [`InvalidPolicy::ZeroOutputCap`].
    pub const fn validate(self) -> Result<(), InvalidPolicy> {
        if self.timeout_millis == 0 {
            return Err(InvalidPolicy::ZeroTimeout);
        }
        if self.max_output_bytes == 0 {
            return Err(InvalidPolicy::ZeroOutputCap);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    // Tests for the limits domain: a derived zero is a trap, so the default must
    // be non-zero, and a zero field must be rejected.

    use super::*;

    #[test]
    fn the_default_limits_are_non_zero() {
        let limits = Limits::default();
        assert_eq!(limits.timeout_millis, DEFAULT_TIMEOUT_MILLIS);
        assert_eq!(limits.max_output_bytes, DEFAULT_MAX_OUTPUT_BYTES);
        assert!(limits.validate().is_ok());
    }

    #[test]
    fn the_output_cap_default_is_one_mebibyte() {
        // Pinned as a number, not through the constant, so a mutant that changes
        // the arithmetic in `DEFAULT_MAX_OUTPUT_BYTES` is caught.
        assert_eq!(DEFAULT_MAX_OUTPUT_BYTES, 1_048_576);
    }

    #[test]
    fn the_timeout_default_is_five_minutes() {
        assert_eq!(DEFAULT_TIMEOUT_MILLIS, 300_000);
    }

    #[test]
    fn a_zero_timeout_is_rejected() {
        let limits = Limits {
            timeout_millis: 0,
            ..Limits::default()
        };
        assert_eq!(limits.validate(), Err(InvalidPolicy::ZeroTimeout));
    }

    #[test]
    fn a_zero_output_cap_is_rejected() {
        let limits = Limits {
            max_output_bytes: 0,
            ..Limits::default()
        };
        assert_eq!(limits.validate(), Err(InvalidPolicy::ZeroOutputCap));
    }

    #[test]
    fn the_timeout_is_derived_from_milliseconds() {
        let limits = Limits {
            timeout_millis: 1500,
            ..Limits::default()
        };
        assert_eq!(limits.timeout(), Duration::from_millis(1500));
    }
}
