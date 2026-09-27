//! The bus core: store, broker, and cursors wired into one state machine.
//!
//! Implements the request semantics from `docs/event-bus/`:
//!
//! - [`Bus::publish`] assigns a `CloudEvents` envelope's bus-owned attributes,
//!   appends it durably, then fans the committed record out to subscribers. It
//!   is acknowledged only after the append's `fsync`, never after fan-out.
//! - [`Bus::subscribe`] resolves the start of delivery to
//!   `max(requested, stored cursor)` and registers the subscriber, so a resume
//!   never silently skips an acknowledged event.
//! - [`Bus::ack`] durably records a subscriber's progress.
//! - [`Bus::replay`] replays committed envelopes from a sequence number.
//!
//! The core is synchronous: it owns the single-writer store and the broker and
//! holds no locks. The async connection loop that drives it over the transport
//! is a separate layer, so the semantics can be tested without a runtime.

use std::path::Path;

use time::OffsetDateTime;

use crate::broker::{Broker, CloseCode, Event as BrokerEvent, SubscriberId, Subscription};
use crate::cloudevent::{Event, Incoming, Sequence};
use crate::cursor::{CursorKey, CursorStore};
use crate::wal::{Store, StoreError};

/// The bus instance URI assigned to every event's `source`.
pub const DEFAULT_SOURCE: &str = "agent://eventbus";

/// A committed publish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Published {
    /// The assigned sequence number.
    pub seq: u64,
    /// The commit timestamp in nanoseconds since the Unix epoch.
    pub timestamp_ns: u64,
    /// The subscribers evicted for being slow, which the caller must close.
    pub evicted: Evicted,
}

/// Subscribers evicted by one publish.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Evicted {
    /// The evicted subscribers.
    subscribers: Vec<SubscriberId>,
}

impl Evicted {
    /// Whether any subscriber was evicted.
    #[must_use]
    pub const fn any(&self) -> bool {
        !self.subscribers.is_empty()
    }

    /// The evicted subscribers.
    #[must_use]
    pub fn subscribers(&self) -> &[SubscriberId] {
        &self.subscribers
    }
}

/// A resolved subscription.
#[derive(Debug)]
pub struct Subscribed {
    /// The subscriber handle, fed by [`Bus::publish`] and [`Bus::replay`].
    pub subscription: Subscription,
    /// The sequence delivery starts at, after the stored cursor is applied.
    pub from_seq: u64,
}

/// The bus: a single-writer store, a broker, and a cursor store.
pub struct Bus {
    store: Store,
    broker: Broker,
    cursors: CursorStore,
    source: String,
    clock: fn() -> OffsetDateTime,
}

impl std::fmt::Debug for Bus {
    fn fmt(
        &self,
        f: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        f.debug_struct("Bus")
            .field("source", &self.source)
            .field("head_seq", &self.head_seq())
            .field("subscribers", &self.broker.subscriber_count())
            .finish_non_exhaustive()
    }
}

impl Bus {
    /// Open a bus over `dir`, using `dir/log` for the log and `dir/cursors` for
    /// cursors, with the default source and a queue capacity per subscriber.
    ///
    /// # Errors
    ///
    /// Returns a [`BusError`] if the log or cursor store cannot be opened.
    pub fn open(
        dir: impl AsRef<Path>,
        capacity: usize,
    ) -> Result<Self, BusError> {
        Self::with_source(dir, capacity, DEFAULT_SOURCE, OffsetDateTime::now_utc)
    }

    /// Open a bus with an explicit source and clock.
    ///
    /// The clock is a function so tests can pin the commit time.
    ///
    /// # Errors
    ///
    /// Returns a [`BusError`] if the log or cursor store cannot be opened.
    pub fn with_source(
        dir: impl AsRef<Path>,
        capacity: usize,
        source: impl Into<String>,
        clock: fn() -> OffsetDateTime,
    ) -> Result<Self, BusError> {
        let dir = dir.as_ref();
        Ok(Self {
            store: Store::open(dir.join("log"))?,
            broker: Broker::new(capacity),
            cursors: CursorStore::open(dir.join("cursors"))?,
            source: source.into(),
            clock,
        })
    }

    /// The bus instance URI.
    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }

    /// The sequence number of the last committed event, if any.
    #[must_use]
    pub const fn head_seq(&self) -> Option<u64> {
        self.store.head_seq()
    }

    /// The number of currently registered subscribers.
    #[must_use]
    pub fn subscriber_count(&self) -> usize {
        self.broker.subscriber_count()
    }

    /// Publish an event: commit it durably, then fan it out.
    ///
    /// The envelope's `source`, `sequence`, and `time` are the bus's; a producer
    /// that set `source` or `sequence` is rejected. The event is assigned the
    /// next sequence and written in one `fsync`, and only then is it delivered.
    ///
    /// The envelope's `time` and the record's frame `timestamp_ns` are different
    /// fields by design: `time` is the *event* time (the producer's, or the
    /// commit time when omitted), while `timestamp_ns` is when the bus wrote the
    /// record. A producer-supplied `time` is therefore allowed to differ from
    /// the frame timestamp.
    ///
    /// # Errors
    ///
    /// Returns a [`BusError`] if the envelope is invalid or the append fails.
    pub fn publish(
        &mut self,
        incoming: Incoming,
    ) -> Result<Published, BusError> {
        let sequence = Sequence::new(self.store.next_seq());
        let event = incoming.commit(&self.source, sequence, (self.clock)())?;
        let payload = event.to_bytes()?;

        let committed = self.store.append(&payload)?;
        let fan_out = self.broker.publish(&BrokerEvent {
            seq: committed.seq,
            timestamp_ns: committed.timestamp_ns,
            payload,
        });

        Ok(Published {
            seq: committed.seq,
            timestamp_ns: committed.timestamp_ns,
            evicted: Evicted {
                subscribers: fan_out.evicted,
            },
        })
    }

    /// Register a subscriber and resolve where its delivery starts.
    ///
    /// The subscriber is identified by `uid` and `subscriber_id`; the start is
    /// `max(from_seq, stored cursor)`. The caller then calls [`Bus::replay`]
    /// from `from_seq` to hand the subscriber history, and [`Bus::publish`]
    /// delivers live events to the same handle.
    ///
    /// # Errors
    ///
    /// Returns a [`BusError`] if the subscriber ID is invalid or the stored
    /// cursor cannot be read.
    pub fn subscribe(
        &mut self,
        uid: u32,
        subscriber_id: &str,
        from_seq: u64,
    ) -> Result<Subscribed, BusError> {
        let key = CursorKey::new(uid, subscriber_id);
        let from_seq = self.cursors.resolve(&key, from_seq)?;
        let subscription = self.broker.subscribe(Self::broker_id(&key));
        Ok(Subscribed {
            subscription,
            from_seq,
        })
    }

    /// Durably record a subscriber's progress.
    ///
    /// # Errors
    ///
    /// Returns a [`BusError`] if the subscriber ID is invalid or the write
    /// fails.
    pub fn ack(
        &self,
        uid: u32,
        subscriber_id: &str,
        cursor: u64,
    ) -> Result<(), BusError> {
        self.cursors
            .store(&CursorKey::new(uid, subscriber_id), cursor)?;
        Ok(())
    }

    /// Replay committed envelopes from `from_seq`, in sequence order.
    ///
    /// # Errors
    ///
    /// Returns a [`BusError`] if the log directory cannot be listed. An envelope
    /// that cannot be decoded, or a segment error, is yielded by the iterator.
    pub fn replay(
        &self,
        from_seq: u64,
    ) -> Result<impl Iterator<Item = Result<Event, BusError>>, BusError> {
        let records = self.store.replay(from_seq)?;
        Ok(records.map(|record| {
            let record = record?;
            Event::from_bytes(&record.payload).map_err(BusError::from)
        }))
    }

    /// The broker identity for a cursor key.
    ///
    /// Two subscribers with the same client ID but different UIDs are distinct,
    /// so the identity includes the UID. The subscriber ID is validated to
    /// exclude `/`, so the joined form is unambiguous.
    fn broker_id(key: &CursorKey) -> SubscriberId {
        SubscriberId::new(format!("{}/{}", key.uid, key.subscriber_id))
    }
}

/// The close code a subscriber observes when the broker evicts it.
pub const SLOW_CONSUMER: CloseCode = CloseCode::SlowConsumer;

/// Failures of a bus operation.
#[derive(Debug, thiserror::Error)]
pub enum BusError {
    /// The write-ahead log failed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The cursor store failed.
    #[error(transparent)]
    Cursor(#[from] crate::cursor::CursorError),
    /// The event envelope was invalid.
    #[error(transparent)]
    Envelope(#[from] crate::cloudevent::Error),
}

#[cfg(test)]
mod tests;
