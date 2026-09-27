//! The set of destinations an egress proxy will open a tunnel to.
//!
//! Matching is **exact** on `host:port`, with the host compared
//! case-insensitively, as `docs/sandbox/permissions.md` specifies. The set is
//! **mutable** at runtime: an authority adds and revokes rules, and the change
//! takes effect for subsequent connections immediately. The event payloads that
//! carry those changes live in [`crate::events`].

use crate::policy::HostPort;

/// A set of permitted egress destinations, mutable while a sandbox runs.
///
/// The zero value permits nothing, so a proxy built with an empty set answers
/// `403` to every destination. That is the deny-by-default the design requires.
/// Add is idempotent and revoke of an absent rule is a no-op, so replaying a
/// `rule_added` / `rule_revoked` log converges to the same set.
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
    ///
    /// Idempotent: adding a destination already present changes nothing, so a
    /// replayed `rule_added` does not grow the set without bound.
    #[must_use]
    pub fn permitting(
        mut self,
        destination: HostPort,
    ) -> Self {
        self.add(destination);
        self
    }

    /// Add `destination` in place. Idempotent.
    pub fn add(
        &mut self,
        destination: HostPort,
    ) {
        if !self.contains(&destination) {
            self.entries.push(destination);
        }
    }

    /// Remove `destination` in place. A destination not present is a no-op, so a
    /// replayed `rule_revoked` is not an error.
    pub fn revoke(
        &mut self,
        destination: &HostPort,
    ) {
        self.entries.retain(|entry| entry != destination);
    }

    /// Whether `host:port` is permitted.
    ///
    /// A destination is permitted only when both the host and the port match one
    /// entry, so a rule never grants more than its own `host:port`.
    #[must_use]
    pub fn permits(
        &self,
        host: &str,
        port: u16,
    ) -> bool {
        self.entries.iter().any(|entry| entry.matches(host, port))
    }

    /// Whether `destination` is present, by exact `host:port`.
    ///
    /// Exact equality here, not [`permits`](Self::permits): a stored entry with a
    /// differently-cased host is a distinct entry to add and revoke, even though
    /// both permit the same connection. That keeps add idempotent and revoke
    /// matched against what was actually added.
    #[must_use]
    pub fn contains(
        &self,
        destination: &HostPort,
    ) -> bool {
        self.entries.contains(destination)
    }

    /// Whether the set permits nothing.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    // Tests for exact destination matching and the idempotent mutations, which
    // are what make replaying a rule log converge.

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

    #[test]
    fn adding_a_new_rule_permits_it() {
        let mut set = DestinationSet::empty();
        set.add(HostPort::new("a.test", 443));
        assert!(set.permits("a.test", 443));
        assert!(!set.permits("a.test", 80), "a rule does not widen its port");
    }

    #[test]
    fn adding_a_present_rule_is_idempotent() {
        let mut set = DestinationSet::empty();
        set.add(HostPort::new("a.test", 443));
        set.add(HostPort::new("a.test", 443));
        assert_eq!(set, DestinationSet::of([HostPort::new("a.test", 443)]));
    }

    #[test]
    fn revoking_a_rule_denies_it_and_leaves_the_rest() {
        let mut set =
            DestinationSet::of([HostPort::new("a.test", 443), HostPort::new("b.test", 443)]);
        set.revoke(&HostPort::new("a.test", 443));
        assert!(!set.permits("a.test", 443), "the revoked rule is gone");
        assert!(set.permits("b.test", 443), "another rule is untouched");
    }

    #[test]
    fn revoking_an_absent_rule_is_a_noop() {
        let mut set = DestinationSet::of([HostPort::new("a.test", 443)]);
        set.revoke(&HostPort::new("absent.test", 443));
        assert_eq!(set, DestinationSet::of([HostPort::new("a.test", 443)]));
    }

    #[test]
    fn contains_is_exact_on_case_even_though_permits_is_not() {
        let set = DestinationSet::of([HostPort::new("A.test", 443)]);
        assert!(set.permits("a.test", 443), "permits is case-insensitive");
        assert!(
            !set.contains(&HostPort::new("a.test", 443)),
            "contains is exact, so a re-cased add is a distinct entry"
        );
        assert!(set.contains(&HostPort::new("A.test", 443)));
    }

    #[test]
    fn add_then_revoke_converges_regardless_of_replays() {
        // Replaying a log of adds and revokes must reach the same set.
        let mut replayed = DestinationSet::empty();
        for _ in 0..3 {
            replayed.add(HostPort::new("a.test", 443));
        }
        for _ in 0..3 {
            replayed.revoke(&HostPort::new("a.test", 443));
        }
        assert!(replayed.is_empty());
    }
}
