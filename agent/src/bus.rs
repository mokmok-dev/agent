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
mod tests {
    // Tests for the bus core: the publish/ack/subscribe loop, durability, resume,
    // and the interaction between the store, broker, and cursors.

    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    /// A unique, empty bus directory removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir()
                .join(format!("agent-bus-{tag}-{}-{unique}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// A pinned commit time, so envelopes are deterministic across a test.
    fn epoch() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(0).expect("the Unix epoch is representable")
    }

    /// An incoming event of `ty` with no producer-set bus attributes.
    fn incoming(ty: &str) -> Incoming {
        serde_json::from_value(serde_json::json!({
            "specversion": "1.0",
            "type": ty,
        }))
        .expect("a minimal valid incoming event")
    }

    /// Open a bus with the pinned clock.
    fn open_bus(dir: &TempDir) -> Bus {
        Bus::with_source(&dir.0, 16, DEFAULT_SOURCE, epoch).expect("opens")
    }

    #[test]
    fn an_empty_bus_has_no_head() {
        let dir = TempDir::new("empty");
        let bus = open_bus(&dir);
        assert_eq!(bus.head_seq(), None);
        assert_eq!(bus.source(), DEFAULT_SOURCE);
        assert_eq!(bus.subscriber_count(), 0);
    }

    #[test]
    fn publish_assigns_a_sequence_and_commits_the_envelope() {
        let dir = TempDir::new("publish");
        let mut bus = open_bus(&dir);

        let first = bus
            .publish(incoming("agent.task.started"))
            .expect("publishes");
        let second = bus
            .publish(incoming("agent.task.started"))
            .expect("publishes");

        assert_eq!(first.seq, 0);
        assert_eq!(second.seq, 1);
        assert_eq!(bus.head_seq(), Some(1));

        // The committed envelope carries the bus-owned attributes.
        let events: Vec<Event> = bus
            .replay(0)
            .expect("lists")
            .map(|event| event.expect("decodes"))
            .collect();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].source, DEFAULT_SOURCE);
        assert_eq!(events[0].sequence.get(), 0);
        assert_eq!(events[1].sequence.get(), 1);
        assert_eq!(events[0].time, Some(epoch()));
    }

    #[test]
    fn a_published_event_survives_a_reopen() {
        let dir = TempDir::new("durable");
        {
            let mut bus = open_bus(&dir);
            bus.publish(incoming("e")).expect("publishes");
            bus.publish(incoming("e")).expect("publishes");
        }

        let bus = open_bus(&dir);
        assert_eq!(bus.head_seq(), Some(1));
        assert_eq!(bus.replay(0).expect("lists").count(), 2);
    }

    #[test]
    fn a_subscriber_receives_a_published_event() {
        let dir = TempDir::new("deliver");
        let mut bus = open_bus(&dir);
        let mut subscribed = bus.subscribe(1000, "audit", 0).expect("subscribes");
        assert_eq!(subscribed.from_seq, 0);

        bus.publish(incoming("agent.task.started"))
            .expect("publishes");

        let delivered = subscribed
            .subscription
            .try_recv()
            .expect("the event is delivered");
        assert_eq!(delivered.seq, 0);
    }

    #[test]
    fn publish_rejects_a_producer_supplied_sequence() {
        let dir = TempDir::new("reserved");
        let mut bus = open_bus(&dir);

        let json = serde_json::json!({
            "specversion": "1.0",
            "type": "e",
            "sequence": "00000000000000000001",
        });
        let bad: Incoming = serde_json::from_value(json).expect("parses as incoming");

        assert!(matches!(bus.publish(bad), Err(BusError::Envelope(_))));
        assert_eq!(bus.head_seq(), None, "nothing was committed");
    }

    #[test]
    fn ack_is_durable_and_makes_a_resume_start_from_the_cursor() {
        let dir = TempDir::new("resume");
        {
            let mut bus = open_bus(&dir);
            let _ = bus.subscribe(1000, "audit", 0).expect("subscribes");
            for _ in 0..3 {
                bus.publish(incoming("e")).expect("publishes");
            }
            bus.ack(1000, "audit", 2).expect("acks");
        }

        // A new connection for the same subscriber resumes from the stored cursor,
        // even though the client asks for an earlier sequence.
        let mut bus = open_bus(&dir);
        let subscribed = bus.subscribe(1000, "audit", 0).expect("subscribes");
        assert_eq!(subscribed.from_seq, 2, "resume never skips the cursor");

        // A client may also ask for a later point.
        let ahead = bus.subscribe(1000, "audit", 9).expect("subscribes");
        assert_eq!(ahead.from_seq, 9);
    }

    #[test]
    fn subscribers_with_the_same_id_on_different_uids_are_distinct() {
        let dir = TempDir::new("uids");
        let mut bus = open_bus(&dir);

        let _first = bus.subscribe(1000, "audit", 0).expect("subscribes");
        let _second = bus.subscribe(2000, "audit", 0).expect("subscribes");
        assert_eq!(bus.subscriber_count(), 2);

        // Their cursors are independent.
        bus.ack(1000, "audit", 5).expect("acks");
        assert_eq!(
            bus.subscribe(1000, "audit", 0)
                .expect("subscribes")
                .from_seq,
            5
        );
        assert_eq!(
            bus.subscribe(2000, "audit", 0)
                .expect("subscribes")
                .from_seq,
            0
        );
    }

    #[test]
    fn a_slow_subscriber_is_evicted_without_blocking_publish() {
        let dir = TempDir::new("slow");
        // Capacity two: the third publish overflows an unread queue.
        let mut bus = Bus::with_source(&dir.0, 2, DEFAULT_SOURCE, epoch).expect("opens");
        let slow = bus.subscribe(1000, "slow", 0).expect("subscribes");

        for _ in 0..3 {
            bus.publish(incoming("e")).expect("publishes");
        }

        assert!(slow.subscription.is_evicted());
        assert_eq!(slow.subscription.close_code(), Some(SLOW_CONSUMER));
        // The write path stayed healthy; all three events are durable.
        assert_eq!(bus.head_seq(), Some(2));
    }

    #[test]
    fn publish_reports_evicted_subscribers() {
        let dir = TempDir::new("evicted");
        let mut bus = Bus::with_source(&dir.0, 1, DEFAULT_SOURCE, epoch).expect("opens");
        let _slow = bus.subscribe(1000, "slow", 0).expect("subscribes");

        bus.publish(incoming("e")).expect("publishes");
        let second = bus.publish(incoming("e")).expect("publishes");

        assert!(second.evicted.any());
        assert_eq!(second.evicted.subscribers().len(), 1);
    }

    #[test]
    fn publish_reports_no_eviction_when_every_subscriber_keeps_up() {
        let dir = TempDir::new("no-eviction");
        let mut bus = open_bus(&dir);
        let mut subscribed = bus.subscribe(1000, "keep-up", 0).expect("subscribes");

        let published = bus.publish(incoming("e")).expect("publishes");
        assert!(!published.evicted.any(), "nobody was evicted");
        assert!(published.evicted.subscribers().is_empty());

        // Drain, so the queue never fills, and publish again.
        assert_eq!(
            subscribed.subscription.try_recv().map(|event| event.seq),
            Some(0)
        );
        let second = bus.publish(incoming("e")).expect("publishes");
        assert!(!second.evicted.any());
    }

    #[test]
    fn replay_from_a_cursor_redelivers_the_acknowledged_event() {
        let dir = TempDir::new("redeliver");
        let mut bus = open_bus(&dir);
        for _ in 0..3 {
            bus.publish(incoming("e")).expect("publishes");
        }

        // Delivery is at-least-once: replaying from the stored cursor 1 includes 1.
        let sequences: Vec<u64> = bus
            .replay(1)
            .expect("lists")
            .map(|event| event.expect("decodes").sequence.get())
            .collect();
        assert_eq!(sequences, [1, 2]);
    }

    #[test]
    fn an_invalid_subscriber_id_is_rejected() {
        let dir = TempDir::new("bad-id");
        let mut bus = open_bus(&dir);
        assert!(matches!(
            bus.subscribe(1000, "../escape", 0),
            Err(BusError::Cursor(_))
        ));
    }

    #[test]
    fn a_non_envelope_record_in_the_log_is_reported_by_replay() {
        let dir = TempDir::new("corrupt-envelope");
        let mut bus = open_bus(&dir);
        bus.publish(incoming("e")).expect("publishes");
        drop(bus);

        // Append a record whose payload is not a CloudEvent, bypassing the bus. The
        // store accepts any bytes; only the envelope decoder rejects them.
        let mut store = Store::open(dir.0.join("log")).expect("opens the log");
        store.append(b"not json").expect("appends");

        let bus = open_bus(&dir);
        let results: Vec<_> = bus.replay(0).expect("lists").collect();
        assert!(results[0].is_ok(), "the real envelope decodes");
        assert!(matches!(results[1], Err(BusError::Envelope(_))));
    }
}
