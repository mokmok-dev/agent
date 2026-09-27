//! The mutable egress allowlist: destination rules that change while a sandbox
//! runs, and the tunnels a revoke must close.
//!
//! A kernel network policy is frozen at spawn; this set is not. An authority adds
//! or revokes a `host:port` rule and the change takes effect for subsequent
//! connections immediately, because the proxy consults this set per connection.
//! Revoking a rule also closes every established tunnel that rule granted, so the
//! reachable set reflects the present rather than a stale past. See
//! `docs/sandbox/permissions.md`.
//!
//! # Why the tunnels live here
//!
//! A revoke is a statement about the present, so a tunnel the rule granted must
//! not survive it. Keeping the live tunnels in the same guarded value as the
//! rules means a revoke sees and closes exactly the tunnels whose rule it removed,
//! with no second lock to order against the first.
//!
//! # Lock protocol
//!
//! One `Mutex` guards both the rules and the tunnel registry, so a revoke is
//! atomic across them. Critical sections are short and never call a closer: the
//! closers are drained and invoked *after* the guard is released, so a closer
//! cannot deadlock against the allowlist. A poisoned lock fails **closed** —
//! `permits` returns `false` — because a panic in a critical section should deny
//! egress, not grant it.

use std::collections::BTreeMap;
use std::sync::Mutex;

use crate::policy::HostPort;

use super::DestinationSet;

/// How to abort one established tunnel: shut both ends so its copy threads end.
///
/// Boxed rather than a concrete socket pair so the allowlist carries no transport
/// type and a test can register a plain counter as the closer.
pub type TunnelCloser = Box<dyn Fn() + Send>;

/// The destinations a running sandbox may reach, mutable at runtime.
pub struct Allowlist {
    inner: Mutex<Inner>,
}

/// The guarded state: the rules and the tunnels currently open under them.
struct Inner {
    rules: DestinationSet,
    tunnels: BTreeMap<u64, LiveTunnel>,
    next_id: u64,
}

/// One established tunnel, and how to abort it.
struct LiveTunnel {
    /// The destination the tunnel was opened to, for matching against a revoke.
    destination: HostPort,
    /// Shuts both ends of the tunnel. Called only on revoke, never on the normal
    /// path, where the copy threads end on their own.
    closer: TunnelCloser,
}

impl Allowlist {
    /// An allowlist that permits nothing.
    #[must_use]
    pub const fn empty() -> Self {
        Self::new(DestinationSet::empty())
    }

    /// An allowlist that starts with `rules`.
    #[must_use]
    pub const fn new(rules: DestinationSet) -> Self {
        Self {
            inner: Mutex::new(Inner {
                rules,
                tunnels: BTreeMap::new(),
                next_id: 0,
            }),
        }
    }

    /// Whether `host:port` is permitted.
    ///
    /// A poisoned lock returns `false`: a panic elsewhere must deny egress, never
    /// grant it.
    #[must_use]
    pub fn permits(
        &self,
        host: &str,
        port: u16,
    ) -> bool {
        self.inner
            .lock()
            .is_ok_and(|inner| inner.rules.permits(host, port))
    }
    /// Add `destination`. Idempotent, so a replayed `rule_added` is a no-op.
    pub fn add(
        &self,
        destination: HostPort,
    ) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.rules.add(destination);
        }
    }

    /// Revoke `destination`, closing every established tunnel it granted.
    ///
    /// Returns how many tunnels were closed. The rule is removed first, then the
    /// matching tunnels are drained and closed with the guard released, so a
    /// closer that calls back into the allowlist cannot deadlock.
    pub fn revoke(
        &self,
        destination: &HostPort,
    ) -> usize {
        let mut closers = Vec::new();
        {
            let Ok(mut inner) = self.inner.lock() else {
                return 0;
            };
            inner.rules.revoke(destination);
            let matches: Vec<u64> = inner
                .tunnels
                .iter()
                .filter(|(_, tunnel)| {
                    tunnel
                        .destination
                        .matches(&destination.host, destination.port)
                })
                .map(|(id, _)| *id)
                .collect();
            for id in matches {
                if let Some(tunnel) = inner.tunnels.remove(&id) {
                    closers.push(tunnel.closer);
                }
            }
        }
        // The closers run with the guard released, so a closer that re-enters the
        // allowlist cannot deadlock.
        let closed = closers.len();
        for closer in closers {
            closer();
        }
        closed
    }

    /// Register an open tunnel, returning its id, or `None` if the lock is
    /// poisoned.
    pub fn register(
        &self,
        destination: HostPort,
        closer: TunnelCloser,
    ) -> Option<u64> {
        let Ok(mut inner) = self.inner.lock() else {
            return None;
        };
        let id = inner.next_id;
        inner.next_id += 1;
        inner.tunnels.insert(
            id,
            LiveTunnel {
                destination,
                closer,
            },
        );
        Some(id)
    }

    /// Forget a closed tunnel, so a later revoke does not call its closer.
    pub fn deregister(
        &self,
        id: u64,
    ) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.tunnels.remove(&id);
        }
    }

    /// A snapshot of the current rules, for tests and for a caller that wants the
    /// set without the tunnel registry.
    #[must_use]
    pub fn rules(&self) -> DestinationSet {
        self.inner
            .lock()
            .map_or_else(|_| DestinationSet::empty(), |inner| inner.rules.clone())
    }

    /// How many tunnels are currently registered.
    #[must_use]
    pub fn open_tunnels(&self) -> usize {
        self.inner.lock().map_or(0, |inner| inner.tunnels.len())
    }
}

impl std::fmt::Debug for Allowlist {
    fn fmt(
        &self,
        f: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        match self.inner.lock() {
            Ok(inner) => f
                .debug_struct("Allowlist")
                .field("rules", &inner.rules)
                .field("open_tunnels", &inner.tunnels.len())
                .finish(),
            Err(_) => f
                .debug_struct("Allowlist")
                .field("poisoned", &true)
                .finish(),
        }
    }
}

#[cfg(test)]
mod tests {
    // Tests for the runtime mutation and, above all, that a revoke closes exactly
    // the tunnels its rule granted. The closers are counters, so the logic is
    // covered without a socket; the integration test drives it with a real tunnel.

    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// A closer that counts invocations.
    fn counter(closed: &Arc<AtomicUsize>) -> TunnelCloser {
        let closed = Arc::clone(closed);
        Box::new(move || {
            closed.fetch_add(1, Ordering::SeqCst);
        })
    }

    fn dest() -> HostPort {
        HostPort::new("api.example.com", 443)
    }

    #[test]
    fn an_empty_allowlist_permits_nothing() {
        assert!(!Allowlist::empty().permits("api.example.com", 443));
    }

    #[test]
    fn a_rule_starts_denied_until_it_is_added() {
        let list = Allowlist::empty();
        list.add(dest());
        assert!(list.permits("api.example.com", 443));
        assert!(!list.permits("api.example.com", 80));
        assert!(!list.permits("other.example.com", 443));
    }

    #[test]
    fn a_revoked_rule_no_longer_permits() {
        let list = Allowlist::empty();
        list.add(dest());
        list.revoke(&dest());
        assert!(!list.permits("api.example.com", 443));
    }

    #[test]
    fn the_rules_snapshot_reflects_add_and_revoke() {
        let list = Allowlist::empty();
        list.add(dest());
        assert_eq!(list.rules(), DestinationSet::of([dest()]));
        list.revoke(&dest());
        assert!(list.rules().is_empty());
    }

    #[test]
    fn revoking_a_rule_closes_a_tunnel_it_granted() {
        let closed = Arc::new(AtomicUsize::new(0));
        let list = Allowlist::empty();
        list.add(dest());
        list.register(dest(), counter(&closed));
        assert_eq!(list.open_tunnels(), 1);

        let count = list.revoke(&dest());
        assert_eq!(count, 1, "one tunnel was closed");
        assert_eq!(closed.load(Ordering::SeqCst), 1, "the closer ran");
        assert_eq!(list.open_tunnels(), 0, "and it was forgotten");
    }

    #[test]
    fn revoking_a_rule_leaves_a_tunnel_to_another_destination_open() {
        let closed = Arc::new(AtomicUsize::new(0));
        let list = Allowlist::empty();
        list.add(dest());
        list.add(HostPort::new("other.test", 443));
        list.register(dest(), counter(&closed));
        list.register(HostPort::new("other.test", 443), counter(&closed));

        let count = list.revoke(&dest());
        assert_eq!(count, 1, "only the matching tunnel is closed");
        assert_eq!(closed.load(Ordering::SeqCst), 1);
        assert_eq!(list.open_tunnels(), 1, "the other tunnel survives");
    }

    #[test]
    fn revoking_a_rule_closes_every_matching_tunnel() {
        // The same destination opened twice: both must close.
        let closed = Arc::new(AtomicUsize::new(0));
        let list = Allowlist::empty();
        list.add(dest());
        list.register(dest(), counter(&closed));
        list.register(dest(), counter(&closed));
        assert_eq!(list.revoke(&dest()), 2);
        assert_eq!(closed.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_deregistered_tunnel_is_not_closed_by_a_later_revoke() {
        let closed = Arc::new(AtomicUsize::new(0));
        let list = Allowlist::empty();
        list.add(dest());
        let id = list.register(dest(), counter(&closed)).expect("registers");
        list.deregister(id);
        assert_eq!(list.revoke(&dest()), 0, "nothing left to close");
        assert_eq!(closed.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn the_revoke_match_is_case_insensitive_on_the_host() {
        let closed = Arc::new(AtomicUsize::new(0));
        let list = Allowlist::empty();
        list.add(HostPort::new("API.example.com", 443));
        list.register(HostPort::new("api.example.com", 443), counter(&closed));
        assert_eq!(list.revoke(&HostPort::new("Api.Example.COM", 443)), 1);
        assert_eq!(closed.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_different_port_is_not_revoked() {
        let closed = Arc::new(AtomicUsize::new(0));
        let list = Allowlist::empty();
        list.add(HostPort::new("api.example.com", 443));
        list.register(HostPort::new("api.example.com", 8443), counter(&closed));
        assert_eq!(list.revoke(&HostPort::new("api.example.com", 443)), 0);
        assert_eq!(closed.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn the_debug_summary_reports_rule_and_tunnel_counts() {
        let closed = Arc::new(AtomicUsize::new(0));
        let list = Allowlist::empty();
        list.add(dest());
        list.register(dest(), counter(&closed));
        let debug = format!("{list:?}");
        assert!(debug.contains("open_tunnels: 1"), "debug was: {debug}");
    }
}
