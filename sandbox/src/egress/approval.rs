//! Per-connection approval: correlating an approver's decision with the request
//! that asked for it.
//!
//! When a connection arrives for a destination not on the [`Allowlist`] and an
//! approver is configured, the proxy asks instead of refusing. It publishes an
//! `egress.requested` event carrying a [`RequestId`], then waits, bounded by a
//! deadline, for an approver to publish `granted`, `denied`, or `cancelled` with
//! the **same** id. See `docs/sandbox/permissions.md`.
//!
//! # Correlation
//!
//! Decisions are matched strictly by [`RequestId`]. A decision with an id the
//! proxy did not open, or one it already resolved, is ignored — so a stale or
//! forged decision releases nothing, and one request cannot release another even
//! with concurrent connections.
//!
//! # A deadline is a `cancelled`, not a `denied`
//!
//! The audit log must never claim an operator refused something they never saw.
//! A wait that times out resolves to [`Approval::Cancelled`], which is what
//! [`await_decision`] returns on any error. The proxy then answers `403`, but the
//! recorded reason is a cancellation.
//!
//! # What lives where
//!
//! This module is the pure core: the id, the registry, the consultation rule, and
//! the bounded wait. The transport that publishes the events onto the event bus is
//! the daemon's, which owns both crates; the sandbox only authors the events.
//!
//! [`Allowlist`]: super::Allowlist

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

/// A UUID correlating one permission request with exactly one decision.
///
/// A newtype over a string the caller supplies: the daemon owns id generation
/// (the bus crate already depends on `ulid`), so the sandbox stays free of an id
/// crate and a request id remains opaque here.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RequestId(String);

impl RequestId {
    /// A request id with the given opaque value.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The id's value.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for RequestId {
    fn fmt(
        &self,
        f: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// An approver's answer to a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Approval {
    /// The destination is allowed; the proxy opens the tunnel.
    Granted,
    /// The destination is refused; the proxy answers `403`.
    Denied,
    /// Nobody decided before the deadline, or an approver withdrew the request.
    /// The proxy answers `403`, but the recorded reason is a cancellation.
    Cancelled,
}

/// What the proxy should do for a connection, given the two things it knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Consultation {
    /// The destination is on the allowlist: tunnel it, never consult an approver.
    Tunnel,
    /// The destination is unlisted and no approver is configured: refuse it. This
    /// is the default, and it avoids prompt fatigue.
    Refuse,
    /// The destination is unlisted and an approver is configured: publish a
    /// `requested` event and wait for a decision.
    Ask,
}

/// Decide what to do for a connection from the allowlist and approver state.
///
/// A listed destination is the pre-approved set, so it never consults an
/// approver. An unlisted destination is refused when no approver is configured,
/// and asked about when one is.
#[must_use]
pub const fn consult(
    listed: bool,
    approver_configured: bool,
) -> Consultation {
    match (listed, approver_configured) {
        (true, _) => Consultation::Tunnel,
        (false, false) => Consultation::Refuse,
        (false, true) => Consultation::Ask,
    }
}

/// The requests currently awaiting a decision, keyed by id.
///
/// One `Mutex` guards the map. Critical sections are short and never block: a
/// decision is a non-blocking `send` on a channel, and the waiter is the one
/// blocking, on its own thread. A poisoned lock fails **closed**: a decision
/// cannot be delivered, so the waiter's deadline records a `cancelled`.
#[derive(Debug, Default)]
pub struct Pending {
    waiters: Mutex<HashMap<RequestId, Sender<Approval>>>,
}

impl Pending {
    /// A registry with no outstanding requests.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `id` and return the receiver the connection waits on.
    ///
    /// Opening an id that is already open replaces the entry; the previous
    /// receiver then sees the channel close and resolves to a cancellation. An id
    /// is unique per request, so this is a guard against a caller bug rather than
    /// an expected path.
    #[must_use]
    pub fn open(
        &self,
        id: RequestId,
    ) -> Receiver<Approval> {
        let (sender, receiver) = std::sync::mpsc::channel();
        if let Ok(mut waiters) = self.waiters.lock() {
            waiters.insert(id, sender);
        }
        receiver
    }

    /// Deliver `approval` to the request `id`, if it is still awaiting one.
    ///
    /// Returns whether the decision was delivered. `false` means the id was
    /// never opened, was already resolved, or was cancelled — in every case the
    /// decision is ignored, so one request cannot release another.
    pub fn resolve(
        &self,
        id: &RequestId,
        approval: Approval,
    ) -> bool {
        let Ok(mut waiters) = self.waiters.lock() else {
            return false;
        };
        // Remove first, so a second decision for the same id finds nothing and is
        // ignored rather than delivered twice.
        waiters
            .remove(id)
            .is_some_and(|sender| sender.send(approval).is_ok())
    }

    /// Remove a request whose wait has ended, without delivering a decision.
    ///
    /// Called when the wait times out, so the entry does not linger. Returns
    /// whether the request was still awaiting.
    pub fn cancel(
        &self,
        id: &RequestId,
    ) -> bool {
        self.waiters
            .lock()
            .is_ok_and(|mut waiters| waiters.remove(id).is_some())
    }

    /// Whether `id` is still awaiting a decision.
    #[must_use]
    pub fn is_awaiting(
        &self,
        id: &RequestId,
    ) -> bool {
        self.waiters
            .lock()
            .is_ok_and(|waiters| waiters.contains_key(id))
    }

    /// How many requests are outstanding.
    #[must_use]
    pub fn len(&self) -> usize {
        self.waiters.lock().map_or(0, |waiters| waiters.len())
    }

    /// Whether no request is outstanding.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Wait up to `timeout` for a decision, resolving to [`Approval::Cancelled`] on
/// any failure.
///
/// A timeout, or a channel that closed because the sender was dropped, both mean
/// nobody decided in time; both record a cancellation, never a denial. This is
/// the one place the "a deadline is a cancelled" rule is enforced.
#[must_use]
pub fn await_decision(
    receiver: &Receiver<Approval>,
    timeout: Duration,
) -> Approval {
    match receiver.recv_timeout(timeout) {
        Ok(approval) => approval,
        Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => Approval::Cancelled,
    }
}

#[cfg(test)]
mod tests {
    // Tests for the consultation rule, the correlation registry, and the
    // deadline rule. Everything here is pure: no socket, no approver, no clock.

    use super::*;

    fn id(value: &str) -> RequestId {
        RequestId::new(value)
    }

    #[test]
    fn a_listed_destination_is_tunnelled_without_an_approver() {
        assert_eq!(consult(true, false), Consultation::Tunnel);
        assert_eq!(consult(true, true), Consultation::Tunnel);
    }

    #[test]
    fn an_unlisted_destination_is_refused_without_an_approver() {
        assert_eq!(consult(false, false), Consultation::Refuse);
    }

    #[test]
    fn an_unlisted_destination_is_asked_about_with_an_approver() {
        assert_eq!(consult(false, true), Consultation::Ask);
    }

    #[test]
    fn a_decision_with_the_right_id_is_delivered() {
        let pending = Pending::new();
        let receiver = pending.open(id("req-1"));
        assert!(pending.is_awaiting(&id("req-1")));

        assert!(
            pending.resolve(&id("req-1"), Approval::Granted),
            "delivered"
        );
        assert_eq!(
            await_decision(&receiver, Duration::from_millis(1)),
            Approval::Granted
        );
    }

    #[test]
    fn a_decision_with_a_different_id_is_ignored() {
        let pending = Pending::new();
        let receiver = pending.open(id("req-1"));

        assert!(
            !pending.resolve(&id("req-2"), Approval::Granted),
            "an id nobody opened releases nothing"
        );
        // The original request is still awaiting.
        assert!(pending.is_awaiting(&id("req-1")));
        // And its wait times out to a cancellation, not the stray grant.
        assert_eq!(
            await_decision(&receiver, Duration::from_millis(1)),
            Approval::Cancelled
        );
    }

    #[test]
    fn a_second_decision_for_the_same_id_is_ignored() {
        let pending = Pending::new();
        let _receiver = pending.open(id("req-1"));
        assert!(pending.resolve(&id("req-1"), Approval::Denied));
        assert!(
            !pending.resolve(&id("req-1"), Approval::Granted),
            "the first decision wins; the second is ignored"
        );
    }

    #[test]
    fn one_request_cannot_release_another() {
        let pending = Pending::new();
        let first = pending.open(id("req-1"));
        let second = pending.open(id("req-2"));

        assert!(pending.resolve(&id("req-2"), Approval::Granted));
        assert_eq!(
            await_decision(&second, Duration::from_millis(1)),
            Approval::Granted
        );
        // The first is untouched and cancels.
        assert_eq!(
            await_decision(&first, Duration::from_millis(1)),
            Approval::Cancelled
        );
    }

    #[test]
    fn a_deadline_records_a_cancellation_not_a_denial() {
        let pending = Pending::new();
        let receiver = pending.open(id("req-1"));
        let approval = await_decision(&receiver, Duration::from_millis(1));
        assert_eq!(
            approval,
            Approval::Cancelled,
            "a timeout must never be recorded as a denial"
        );
    }

    #[test]
    fn cancelling_removes_a_request_without_delivering() {
        let pending = Pending::new();
        let _receiver = pending.open(id("req-1"));
        assert!(pending.cancel(&id("req-1")), "the request was awaiting");
        assert!(!pending.is_awaiting(&id("req-1")));
        assert!(
            !pending.resolve(&id("req-1"), Approval::Granted),
            "a cancelled request cannot be resolved later"
        );
    }

    #[test]
    fn cancelling_an_unknown_request_is_a_noop() {
        let pending = Pending::new();
        assert!(!pending.cancel(&id("absent")));
    }

    #[test]
    fn the_registry_tracks_its_outstanding_requests() {
        let pending = Pending::new();
        assert!(pending.is_empty());
        let _a = pending.open(id("req-1"));
        let _b = pending.open(id("req-2"));
        assert_eq!(pending.len(), 2);
        assert!(!pending.is_empty());
        pending.resolve(&id("req-1"), Approval::Denied);
        assert_eq!(pending.len(), 1, "a resolved request leaves the registry");
    }

    #[test]
    fn a_denied_decision_is_delivered_as_denied() {
        let pending = Pending::new();
        let receiver = pending.open(id("req-1"));
        assert!(pending.resolve(&id("req-1"), Approval::Denied));
        assert_eq!(
            await_decision(&receiver, Duration::from_millis(1)),
            Approval::Denied
        );
    }

    #[test]
    fn an_id_keeps_its_value_and_displays_it() {
        let id = id("req-9");
        assert_eq!(id.as_str(), "req-9");
        assert_eq!(id.to_string(), "req-9");
        assert_eq!(id, RequestId::new("req-9"));
        assert_ne!(id, RequestId::new("req-10"));
    }

    #[test]
    fn reopening_an_id_replaces_the_waiter() {
        // A caller bug: the same id opened twice. The first receiver is dropped
        // and resolves to a cancellation; only the second gets the decision.
        let pending = Pending::new();
        let first = pending.open(id("req-1"));
        let second = pending.open(id("req-1"));
        assert_eq!(pending.len(), 1);
        assert!(pending.resolve(&id("req-1"), Approval::Granted));
        assert_eq!(
            await_decision(&second, Duration::from_millis(1)),
            Approval::Granted
        );
        assert_eq!(
            await_decision(&first, Duration::from_millis(1)),
            Approval::Cancelled
        );
    }
}
