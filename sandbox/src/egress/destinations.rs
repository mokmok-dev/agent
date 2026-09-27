//! The set of destinations an egress proxy will open a tunnel to.
//!
//! Matching is **exact** on `host:port`, with the host compared
//! case-insensitively, as `docs/sandbox/permissions.md` specifies. The set is
//! immutable here; milestone 5 wraps it in the mutable allowlist the runtime
//! permission changes write to.

use crate::policy::HostPort;

/// A set of permitted egress destinations.
///
/// The zero value permits nothing, so a proxy built with an empty set answers
/// `403` to every destination. That is the deny-by-default the design requires.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DestinationSet {
    entries: Vec<HostPort>,
}

impl DestinationSet {
    /// An empty set, which permits nothing.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// A set containing every entry of `entries`.
    #[must_use]
    pub fn of(entries: impl IntoIterator<Item = HostPort>) -> Self {
        Self {
            entries: entries.into_iter().collect(),
        }
    }

    /// Add `destination`, returning the updated set.
    #[must_use]
    pub fn permitting(
        mut self,
        destination: HostPort,
    ) -> Self {
        self.entries.push(destination);
        self
    }

    /// Whether `host:port` is permitted.
    ///
    /// The host is compared case-insensitively; the port exactly. A destination
    /// is permitted only when both match one entry, so a rule never grants more
    /// than its own `host:port`.
    #[must_use]
    pub fn permits(
        &self,
        host: &str,
        port: u16,
    ) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.port == port && entry.host.eq_ignore_ascii_case(host))
    }

    /// Whether the set permits nothing.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    // Tests for exact destination matching, including the two ways a lookalike
    // could be admitted: a case difference and a shared prefix.

    use super::*;

    #[test]
    fn an_empty_set_permits_nothing_and_reports_empty() {
        let set = DestinationSet::empty();
        assert!(!set.permits("api.example.com", 443));
        assert!(set.is_empty());
    }

    #[test]
    fn a_non_empty_set_is_not_empty() {
        // Pins `is_empty` against a mutant that always returns `true`.
        let set = DestinationSet::of([HostPort::new("a.test", 443)]);
        assert!(!set.is_empty());
    }

    #[test]
    fn a_rule_permits_exactly_its_host_and_port() {
        let set = DestinationSet::of([HostPort::new("api.example.com", 443)]);
        assert!(set.permits("api.example.com", 443));
        assert!(!set.permits("api.example.com", 80), "the port must match");
        assert!(
            !set.permits("other.example.com", 443),
            "the host must match"
        );
    }

    #[test]
    fn the_host_is_compared_case_insensitively() {
        let set = DestinationSet::of([HostPort::new("API.example.com", 443)]);
        assert!(set.permits("api.example.com", 443));
        assert!(set.permits("Api.Example.COM", 443));
    }

    #[test]
    fn a_shared_prefix_is_not_a_match() {
        let set = DestinationSet::of([HostPort::new("api.example.com", 443)]);
        assert!(!set.permits("api.example.com.evil.test", 443));
        assert!(!set.permits("api.example.co", 443));
        assert!(!set.permits("xapi.example.com", 443));
    }

    #[test]
    fn several_rules_are_each_exact() {
        let set = DestinationSet::empty()
            .permitting(HostPort::new("a.test", 443))
            .permitting(HostPort::new("b.test", 8443));
        assert!(set.permits("a.test", 443));
        assert!(set.permits("b.test", 8443));
        assert!(!set.permits("a.test", 8443), "ports do not cross rules");
        assert!(!set.permits("b.test", 443));
    }
}
