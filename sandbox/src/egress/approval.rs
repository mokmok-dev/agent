//! Per-connection approval: correlating an approver's decision with the request
//! that asked for it.
//!
//! When a connection arrives for a destination not on the [`Allowlist`] and an
//! approver is configured, the proxy asks instead of refusing. It publishes an
//! `egress.requested` event carrying a [`RequestId`], then waits, bounded by a
//! deadline, for an approver to publish `granted`, `denied`, or `cancelled` with
//! the **same** id. See `docs/sandbox/permissions.md`.
//!
//! # Where the pieces sit
//!
//! - [`Approver`] is the seam the proxy calls: "ask about this destination and
//!   give me an answer". The proxy does not know how the answer is obtained.
//! - [`Desk`] is the sandbox's implementation of that seam. It owns the
//!   correlation and the deadline, and needs only a [`Publisher`] to reach the
//!   event bus.
//! - [`Publisher`] is the daemon's side: publish an authored [`Event`], and mint
//!   a [`RequestId`]. The daemon owns id generation (the bus crate already
//!   depends on an id crate), so the sandbox stays free of one.
//!
//! The event bus transport itself is the daemon's, which owns both crates; the
//! sandbox only authors the events.
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
//! A wait that times out resolves to [`Approval::Cancelled`], and [`Desk`]
//! publishes its own `egress.cancelled` for the deadline — but only when it
//! *timed out*, not when it received an approver's explicit cancellation, so
//! exactly one cancellation is recorded.
//!
//! [`Allowlist`]: super::Allowlist

use std::collections::HashMap;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::events::Event;
use crate::policy::HostPort;

/// A UUID correlating one permission request with exactly one decision.
///
/// A newtype over a string the caller supplies: the daemon owns id generation
/// (the bus crate already depends on an id crate), so the sandbox stays free of
/// an id crate and a request id remains opaque here.
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

/// The result of waiting for a decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// A decision arrived: an approver's `granted`, `denied`, or `cancelled`.
    Decided(Approval),
    /// The deadline passed with no decision. Distinct from a received
    /// `cancelled`, so the caller can record exactly one cancellation.
    Expired,
}

impl Outcome {
    /// The approval to act on, mapping a deadline to [`Approval::Cancelled`].
    #[must_use]
    pub const fn approval(self) -> Approval {
        match self {
            Self::Decided(approval) => approval,
            Self::Expired => Approval::Cancelled,
        }
    }
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

/// The seam the proxy calls to obtain a decision about an unlisted destination.
///
/// The proxy does not know how the answer is obtained. [`Desk`] is the sandbox's
/// implementation, which publishes a `requested` event and waits on the bus; a
/// test can supply a trivial one.
///
/// An implementation must return [`Approval::Cancelled`], never
/// [`Approval::Denied`], when its deadline passes with no decision.
pub trait Approver: Send + Sync + std::fmt::Debug {
    /// Ask about `destination`, blocking no longer than the implementation's
    /// deadline. `traceparent` continues the confined request's trace when the
    /// caller has one.
    fn ask(
        &self,
        destination: &HostPort,
        traceparent: Option<String>,
    ) -> Approval;
}

/// How the sandbox reaches the event bus: publish an authored event, and mint a
/// request id.
///
/// The daemon implements this over the bus crate, which the sandbox does not
/// depend on. Id generation lives here rather than in the sandbox so the sandbox
/// needs no id crate.
pub trait Publisher: Send + Sync + std::fmt::Debug {
    /// Publish `event` onto the bus. The daemon assigns the envelope attributes.
    fn publish(
        &self,
        event: Event,
    );

    /// Mint an id for one approval request.
    fn next_request_id(&self) -> RequestId;
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
    /// Called when the wait ends, so the entry does not linger. Returns whether
    /// the request was still awaiting.
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

/// Wait up to `timeout` for a decision.
///
/// Distinguishes a received decision from an expiring deadline, so the caller
/// can record exactly one cancellation: a received `cancelled` was the
/// approver's, while [`Outcome::Expired`] is the proxy's to record. A channel
/// that closed because the sender was dropped is treated as an expiry: nobody
/// decided in time.
#[must_use]
pub fn await_outcome(
    receiver: &Receiver<Approval>,
    timeout: Duration,
) -> Outcome {
    match receiver.recv_timeout(timeout) {
        Ok(approval) => Outcome::Decided(approval),
        Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => Outcome::Expired,
    }
}

/// Wait up to `timeout`, returning the approval to act on.
///
/// A deadline maps to [`Approval::Cancelled`], never [`Approval::Denied`]. Use
/// [`await_outcome`] when the caller must know whether the deadline expired.
#[must_use]
pub fn await_decision(
    receiver: &Receiver<Approval>,
    timeout: Duration,
) -> Approval {
    await_outcome(receiver, timeout).approval()
}

/// The sandbox's [`Approver`]: publish a `requested` event, wait for the
/// correlated decision, and record the proxy's own cancellation on a deadline.
#[derive(Debug)]
pub struct Desk {
    pending: Pending,
    publisher: Arc<dyn Publisher>,
    sandbox_id: String,
    deadline: Duration,
}

impl Desk {
    /// A desk that asks as `sandbox_id`, waiting up to `deadline` per request.
    #[must_use]
    pub fn new(
        publisher: Arc<dyn Publisher>,
        sandbox_id: impl Into<String>,
        deadline: Duration,
    ) -> Self {
        Self {
            pending: Pending::new(),
            publisher,
            sandbox_id: sandbox_id.into(),
            deadline,
        }
    }

    /// Deliver a decision received from the bus to the request it answers.
    ///
    /// The daemon calls this when it sees an `egress.granted` / `denied` /
    /// `cancelled` event. Returns whether the decision matched an outstanding
    /// request, so a stale or mismatched id is ignored.
    pub fn resolve(
        &self,
        id: &RequestId,
        approval: Approval,
    ) -> bool {
        self.pending.resolve(id, approval)
    }

    /// Whether `id` is still awaiting a decision.
    #[must_use]
    pub fn is_awaiting(
        &self,
        id: &RequestId,
    ) -> bool {
        self.pending.is_awaiting(id)
    }
}

impl Approver for Desk {
    fn ask(
        &self,
        destination: &HostPort,
        traceparent: Option<String>,
    ) -> Approval {
        let id = self.publisher.next_request_id();
        let receiver = self.pending.open(id.clone());
        self.publisher.publish(Event::egress_requested(
            &self.sandbox_id,
            &id,
            destination,
            traceparent.clone(),
        ));
        let outcome = await_outcome(&receiver, self.deadline);
        // The wait has ended; the entry must not linger whether or not it was
        // resolved.
        self.pending.cancel(&id);
        if outcome == Outcome::Expired {
            // The proxy records its own deadline; an approver's explicit
            // `cancelled` was already published by the approver, so this is not a
            // double record.
            self.publisher.publish(Event::egress_cancelled(
                &self.sandbox_id,
                &id,
                destination,
                traceparent,
            ));
        }
        outcome.approval()
    }
}

#[cfg(test)]
mod tests {
    // Tests for the consultation rule, the correlation registry, the deadline
    // rule, and the Desk that ties them together. Everything here is pure: no
    // socket, no bus, no clock beyond a real short deadline.

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc::{Sender, channel};

    use super::*;

    fn id(value: &str) -> RequestId {
        RequestId::new(value)
    }

    fn dest() -> HostPort {
        HostPort::new("api.example.com", 443)
    }

    /// A publisher that records every event and mints sequential ids, with a
    /// channel so a test can react to a published `requested`.
    #[derive(Debug)]
    struct FakePublisher {
        published: Mutex<Vec<Event>>,
        requests: Mutex<Sender<Event>>,
        counter: AtomicUsize,
    }

    impl FakePublisher {
        fn new() -> (Arc<Self>, Receiver<Event>) {
            let (sender, receiver) = channel();
            let publisher = Arc::new(Self {
                published: Mutex::new(Vec::new()),
                requests: Mutex::new(sender),
                counter: AtomicUsize::new(0),
            });
            (publisher, receiver)
        }

        /// The events published so far.
        fn published(&self) -> Vec<Event> {
            self.published.lock().map(|e| e.clone()).unwrap_or_default()
        }

        /// Whether any published event has the given type.
        fn published_type(
            &self,
            ty: &str,
        ) -> bool {
            self.published().iter().any(|event| event.ty == ty)
        }
    }

    impl Publisher for FakePublisher {
        fn publish(
            &self,
            event: Event,
        ) {
            if let Ok(mut published) = self.published.lock() {
                published.push(event.clone());
            }
            if let Ok(sender) = self.requests.lock() {
                let _ = sender.send(event);
            }
        }

        fn next_request_id(&self) -> RequestId {
            let n = self.counter.fetch_add(1, Ordering::SeqCst);
            RequestId::new(format!("req-{n}"))
        }
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
        assert!(pending.is_awaiting(&id("req-1")));
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
        assert_eq!(
            await_decision(&first, Duration::from_millis(1)),
            Approval::Cancelled
        );
    }

    #[test]
    fn a_deadline_expires_and_maps_to_a_cancellation_not_a_denial() {
        let pending = Pending::new();
        let receiver = pending.open(id("req-1"));
        assert_eq!(
            await_outcome(&receiver, Duration::from_millis(1)),
            Outcome::Expired
        );
        assert_eq!(
            Outcome::Expired.approval(),
            Approval::Cancelled,
            "a timeout must never be recorded as a denial"
        );
    }

    #[test]
    fn a_received_cancellation_is_a_decision_not_an_expiry() {
        // Distinct from a deadline, so the caller knows not to record its own.
        let pending = Pending::new();
        let receiver = pending.open(id("req-1"));
        assert!(pending.resolve(&id("req-1"), Approval::Cancelled));
        assert_eq!(
            await_outcome(&receiver, Duration::from_millis(1)),
            Outcome::Decided(Approval::Cancelled)
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
    fn an_id_keeps_its_value_and_displays_it() {
        let id = id("req-9");
        assert_eq!(id.as_str(), "req-9");
        assert_eq!(id.to_string(), "req-9");
        assert_eq!(id, RequestId::new("req-9"));
        assert_ne!(id, RequestId::new("req-10"));
    }

    #[test]
    fn reopening_an_id_replaces_the_waiter() {
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

    #[test]
    fn the_desk_publishes_requested_and_returns_the_grant() {
        let (publisher, requests) = FakePublisher::new();
        let desk = Arc::new(Desk::new(publisher, "sbx-7", Duration::from_secs(5)));

        // A stand-in approver: react to the `requested` event with a grant.
        let resolver = Arc::clone(&desk);
        let handle = std::thread::spawn(move || {
            let event = requests.recv().expect("a requested event");
            assert_eq!(event.ty, crate::events::EGRESS_REQUESTED);
            let id = RequestId::new(event.data["request_id"].as_str().expect("has an id"));
            assert!(resolver.resolve(&id, Approval::Granted));
        });

        assert_eq!(desk.ask(&dest(), None), Approval::Granted);
        handle.join().expect("the resolver joins");
        assert!(
            !desk.is_awaiting(&RequestId::new("req-0")),
            "the request is no longer awaiting after it is answered"
        );
    }

    #[test]
    fn the_desk_records_its_own_cancellation_on_a_deadline() {
        let (publisher, _requests) = FakePublisher::new();
        let desk = Desk::new(publisher.clone(), "sbx-7", Duration::from_millis(1));

        assert_eq!(
            desk.ask(&dest(), None),
            Approval::Cancelled,
            "a deadline is a cancellation"
        );
        assert!(
            publisher.published_type(crate::events::EGRESS_REQUESTED),
            "the ask was published"
        );
        assert!(
            publisher.published_type(crate::events::EGRESS_CANCELLED),
            "the proxy records its own deadline cancellation"
        );
    }

    #[test]
    fn the_desk_does_not_double_record_an_approvers_cancellation() {
        let (publisher, requests) = FakePublisher::new();
        let desk = Arc::new(Desk::new(
            publisher.clone(),
            "sbx-7",
            Duration::from_secs(5),
        ));

        let resolver = Arc::clone(&desk);
        let handle = std::thread::spawn(move || {
            let event = requests.recv().expect("a requested event");
            let id = RequestId::new(event.data["request_id"].as_str().expect("has an id"));
            assert!(resolver.resolve(&id, Approval::Cancelled));
        });

        assert_eq!(desk.ask(&dest(), None), Approval::Cancelled);
        handle.join().expect("the resolver joins");
        assert!(
            !publisher.published_type(crate::events::EGRESS_CANCELLED),
            "the approver's cancellation alone is recorded, not a proxy one too"
        );
    }

    #[test]
    fn the_desk_ignores_a_decision_for_another_request() {
        let (publisher, _requests) = FakePublisher::new();
        let desk = Desk::new(publisher, "sbx-7", Duration::from_millis(1));
        // No resolver: the ask expires. A stray resolve for an unknown id is a
        // no-op and does not release anything.
        assert!(!desk.resolve(&RequestId::new("nobody"), Approval::Granted));
        assert_eq!(desk.ask(&dest(), None), Approval::Cancelled);
    }
}
