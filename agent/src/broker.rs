//! In-process broker: subscriber registry and bounded fan-out.
//!
//! Implements the broker from `docs/event-bus/delivery.md`. The broker receives
//! committed events from the writer and fans them out to subscribers. It never
//! blocks the writer:
//!
//! - Each subscriber owns a bounded queue ([`DEFAULT_QUEUE_CAPACITY`] slots by
//!   default).
//! - Fan-out uses `try_send`, so a full queue never blocks. A subscriber whose
//!   queue is full is *slow* and is evicted with [`CloseCode::SlowConsumer`].
//!   Because the cursor is durable, the client reconnects and resumes from its
//!   last `ack`, and the lost range replays from the log.
//! - `pending` is sent only after the writer's `fsync`, never after fan-out. A
//!   subscriber may therefore be behind the committed head without affecting
//!   durability.
//!
//! The broker is synchronous and runtime-agnostic: it uses only the
//! `try_send`/`try_recv` halves of [`tokio::sync::mpsc`], which need no async
//! runtime. The async subscriber task that awaits the queue arrives with the
//! transport milestone.
//!
//! ```
//! use agent::broker::{Broker, Event, SubscriberId};
//!
//! let mut broker = Broker::new(8);
//! let mut subscription = broker.subscribe(SubscriberId::new("audit-log"));
//!
//! let event = Event { seq: 0, timestamp_ns: 0, payload: b"{}".to_vec() };
//! let fan_out = broker.publish(&event);
//!
//! assert_eq!(fan_out.delivered, 1);
//! assert_eq!(subscription.try_recv().map(|event| event.seq), Some(0));
//! ```

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::mpsc;

/// Default number of unread events a subscriber's queue holds before it is
/// treated as slow.
pub const DEFAULT_QUEUE_CAPACITY: usize = 1024;

/// A subscriber identity, stable across reconnects.
///
/// Per `docs/event-bus/delivery.md`, the identity is a client-supplied stable
/// subscriber ID, namespaced by peer UID where available. This type is the
/// client-supplied part; the UID namespace is applied by the transport.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SubscriberId(String);

impl SubscriberId {
    /// Wrap a client-supplied subscriber ID.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The subscriber ID as a string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for SubscriberId {
    fn from(id: &str) -> Self {
        Self(id.to_owned())
    }
}

impl std::fmt::Display for SubscriberId {
    fn fmt(
        &self,
        f: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A committed event delivered to subscribers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    /// The global sequence number, assigned by the writer.
    pub seq: u64,
    /// The commit timestamp in nanoseconds since the Unix epoch.
    pub timestamp_ns: u64,
    /// The encoded `CloudEvents` JSON payload.
    pub payload: Vec<u8>,
}

/// Why a subscriber was closed by the broker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseCode {
    /// The subscriber's queue filled, so it was evicted and must resume from
    /// its durable cursor.
    SlowConsumer,
}

/// A subscriber handle: a bounded receiver plus a close notification.
#[derive(Debug)]
pub struct Subscription {
    id: SubscriberId,
    receiver: mpsc::Receiver<Event>,
    /// Set when the broker evicts this subscriber.
    evicted: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Subscription {
    /// The subscriber's identity.
    #[must_use]
    pub const fn id(&self) -> &SubscriberId {
        &self.id
    }

    /// Take the next queued event without awaiting.
    ///
    /// Returns `None` when the queue is currently empty. The subscription stays
    /// valid until it is evicted.
    #[must_use]
    pub fn try_recv(&mut self) -> Option<Event> {
        self.receiver.try_recv().ok()
    }

    /// Whether the broker has evicted this subscriber for being slow.
    #[must_use]
    pub fn is_evicted(&self) -> bool {
        self.evicted.load(Ordering::Acquire)
    }

    /// The close code if the subscriber was evicted.
    #[must_use]
    pub fn close_code(&self) -> Option<CloseCode> {
        self.is_evicted().then_some(CloseCode::SlowConsumer)
    }

    /// How many events are queued but unread.
    #[must_use]
    pub fn queued(&self) -> usize {
        self.receiver.len()
    }
}

/// The in-process fan-out broker.
///
/// Register a subscriber with [`Broker::subscribe`], then publish committed
/// events with [`Broker::publish`]. A slow subscriber is evicted rather than
/// allowed to stall the writer.
#[derive(Debug)]
pub struct Broker {
    capacity: usize,
    next_id: AtomicU64,
    subscribers: HashMap<SubscriberId, Subscriber>,
}

/// A registered subscriber's broker-side state.
#[derive(Debug)]
struct Subscriber {
    sender: mpsc::Sender<Event>,
    evicted: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Default for Broker {
    fn default() -> Self {
        Self::new(DEFAULT_QUEUE_CAPACITY)
    }
}

impl Broker {
    /// Create a broker whose subscribers each hold `capacity` unread events.
    ///
    /// A `capacity` of zero is treated as one, because a zero-capacity channel
    /// can never accept a `try_send` and would evict every subscriber
    /// immediately.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            next_id: AtomicU64::new(0),
            subscribers: HashMap::new(),
        }
    }

    /// The per-subscriber queue capacity.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// The number of currently registered subscribers.
    #[must_use]
    pub fn subscriber_count(&self) -> usize {
        self.subscribers.len()
    }

    /// Register a subscriber with an explicit identity.
    ///
    /// If a subscriber with the same ID is already registered, it is replaced;
    /// the previous subscription is evicted so its reader observes the
    /// disconnect.
    pub fn subscribe(
        &mut self,
        id: SubscriberId,
    ) -> Subscription {
        let (sender, receiver) = mpsc::channel(self.capacity);
        let evicted = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        if let Some(previous) = self.subscribers.insert(
            id.clone(),
            Subscriber {
                sender,
                evicted: std::sync::Arc::clone(&evicted),
            },
        ) {
            previous.evicted.store(true, Ordering::Release);
        }

        Subscription {
            id,
            receiver,
            evicted,
        }
    }

    /// Register a subscriber with a generated identity.
    pub fn subscribe_new(&mut self) -> Subscription {
        let id = SubscriberId::new(format!(
            "subscriber-{}",
            self.next_id.fetch_add(1, Ordering::Relaxed)
        ));
        self.subscribe(id)
    }

    /// Remove a subscriber, for example when its connection closes.
    ///
    /// Returns whether a subscriber with that ID was registered.
    pub fn unsubscribe(
        &mut self,
        id: &SubscriberId,
    ) -> bool {
        self.subscribers.remove(id).is_some()
    }

    /// Fan a committed event out to every subscriber.
    ///
    /// This never blocks. A subscriber whose queue is full is evicted and
    /// removed; the returned [`FanOut`] lists the evicted IDs so the caller can
    /// close their connections, and the count actually delivered. The writer
    /// must call this only after its `fsync`, once the event is durable.
    pub fn publish(
        &mut self,
        event: &Event,
    ) -> FanOut {
        let mut evicted = Vec::new();
        let mut delivered = 0usize;

        self.subscribers.retain(|id, subscriber| {
            match subscriber.sender.try_send(event.clone()) {
                Ok(()) => {
                    delivered += 1;
                    true
                },
                Err(mpsc::error::TrySendError::Full(_)) => {
                    // Slow subscriber: evict it rather than block the writer.
                    subscriber.evicted.store(true, Ordering::Release);
                    evicted.push(id.clone());
                    false
                },
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    // The reader is gone; drop the registration.
                    false
                },
            }
        });

        FanOut { delivered, evicted }
    }
}

/// The outcome of one [`Broker::publish`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FanOut {
    /// The number of subscribers the event was queued for.
    pub delivered: usize,
    /// The subscribers evicted for being slow, in arbitrary order.
    pub evicted: Vec<SubscriberId>,
}

impl FanOut {
    /// Whether any subscriber was evicted.
    #[must_use]
    pub const fn evicted_any(&self) -> bool {
        !self.evicted.is_empty()
    }
}

#[cfg(test)]
mod tests;
