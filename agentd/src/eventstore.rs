//! Durable, append-only persistence for the event bus.
//!
//! [`open`] attaches the store to an [`EventBus`] as an ordinary subscriber:
//! a collector task receives events and hands them to a dedicated writer
//! thread that owns the SQLite connection and inserts events in batches.
//!
//! Persistence is best-effort, matching the bus semantics: a store that falls
//! behind the bus records the missed span with a `store.lagged` marker event,
//! and a failing SQLite backend stops persistence while the daemon keeps
//! running.

use crate::events::{Event, EventBus};
use rusqlite::Connection;
use serde_json::json;
use std::path::Path;
use thiserror::Error;
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc;

/// Capacity of the queue between the collector and the writer thread; when
/// the writer falls behind, it bounds the buffered events and backpressures
/// the collector into lagging on the bus like any other subscriber.
const QUEUE_CAPACITY: usize = 1024;

/// Kind of the marker event recorded when the store misses events because it
/// lagged behind the bus; mirrors the `error.lagged` notice WebSocket clients
/// receive for the same condition.
pub const LAGGED_KIND: &str = "store.lagged";

/// The forward-only schema migrations, applied in order at startup. Shipped
/// entries are immutable; schema changes are appended as new entries.
const MIGRATIONS: &[(u32, &str)] = &[(
    1,
    "CREATE TABLE events (
        seq         INTEGER PRIMARY KEY,
        id          TEXT    NOT NULL,
        source      TEXT    NOT NULL,
        specversion TEXT    NOT NULL,
        type        TEXT    NOT NULL,
        time        TEXT,
        event       TEXT    NOT NULL
    ) STRICT",
)];

const INSERT_EVENT_SQL: &str = "INSERT INTO events
    (id, source, specversion, type, time, event)
    VALUES (?1, ?2, ?3, ?4, ?5, ?6)";

/// Errors returned by [`open`].
#[derive(Debug, Error)]
pub enum StoreError {
    /// The database schema is newer than the migrations this build supports.
    #[error("event store schema version {stored} is newer than the supported version {latest}")]
    UnsupportedSchema {
        /// The schema version recorded in the database.
        stored: u32,
        /// The highest schema version this build supports.
        latest: u32,
    },
    /// Opening, configuring, or migrating the SQLite database failed.
    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
    /// Spawning the writer thread failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Opens the SQLite event store at `path`, migrates it to the latest schema
/// version, and attaches it to `bus` as a subscriber.
///
/// Events published on `bus` after this call are persisted as `CloudEvents`
/// by background workers; the workers run until `bus` closes or the process
/// exits, so they do not need to be kept alive by the caller.
///
/// If the SQLite backend fails while writing a batch, persistence stops
/// permanently, the failure is logged, and the daemon keeps running.
///
/// # Errors
///
/// Returns [`StoreError::Sql`] if the database cannot be opened, configured,
/// or migrated, [`StoreError::UnsupportedSchema`] if its schema is newer than
/// this build supports, and [`StoreError::Io`] if its parent directory cannot
/// be created or the writer thread cannot be spawned.
///
/// # Panics
///
/// Panics if called outside a tokio runtime, because the collector runs as a
/// spawned task.
pub fn open(
    path: impl AsRef<Path>,
    bus: &EventBus,
) -> Result<(), StoreError> {
    if let Some(parent) = path.as_ref().parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut connection = Connection::open(path.as_ref())?;
    configure(&connection)?;
    migrate(&mut connection)?;

    let (writer, events) = mpsc::channel(QUEUE_CAPACITY);
    let collector = bus.subscribe();

    std::thread::Builder::new()
        .name(String::from("eventstore"))
        .spawn(move || write_events(connection, events))?;

    tokio::spawn(collect_events(collector, writer));

    Ok(())
}

/// Switches the connection to WAL journaling with `synchronous=NORMAL`,
/// trading some durability against app crashes for write throughput.
fn configure(connection: &Connection) -> Result<(), rusqlite::Error> {
    let journal_mode: String =
        connection.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))?;
    connection.pragma_update(None, "synchronous", "NORMAL")?;
    tracing::debug!(%journal_mode, "configured the event store");
    Ok(())
}

/// Applies the pending [`MIGRATIONS`] in a single transaction each and records
/// the new schema version.
fn migrate(connection: &mut Connection) -> Result<(), StoreError> {
    let stored: u32 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    let latest = MIGRATIONS
        .iter()
        .map(|(version, _)| *version)
        .max()
        .unwrap_or(0);
    if stored > latest {
        return Err(StoreError::UnsupportedSchema { stored, latest });
    }

    for (version, sql) in MIGRATIONS {
        if *version <= stored {
            continue;
        }
        let transaction = connection.transaction()?;
        transaction.execute_batch(sql)?;
        transaction.pragma_update(None, "user_version", version)?;
        transaction.commit()?;
        tracing::info!(to = version, "applied event store schema migration");
    }

    Ok(())
}

/// Consumes batches from `events` and persists each as a single transaction
/// until the channel closes.
///
/// A failed batch stops persistence permanently; the collector observes the
/// dead channel and logs the same condition.
fn write_events(
    mut connection: Connection,
    mut events: mpsc::Receiver<Event>,
) {
    while let Some(first) = events.blocking_recv() {
        let mut batch = vec![first];
        while let Ok(event) = events.try_recv() {
            batch.push(event);
        }

        if let Err(error) = insert_batch(&mut connection, &batch) {
            tracing::error!(%error, "event store failed to persist a batch; stopping persistence");
            return;
        }
    }
}

/// Inserts all `batch` events as one transaction.
fn insert_batch(
    connection: &mut Connection,
    batch: &[Event],
) -> Result<(), rusqlite::Error> {
    let transaction = connection.transaction()?;
    {
        let mut statement = transaction.prepare(INSERT_EVENT_SQL)?;
        for event in batch {
            let envelope = serde_json::to_string(event)
                .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
            statement.execute(rusqlite::params![
                event.id,
                event.source,
                event.specversion,
                event.kind,
                event.time,
                envelope,
            ])?;
        }
    }
    transaction.commit()
}

/// Receives events from the bus and forwards them to the writer thread until
/// the bus closes or the writer stops.
async fn collect_events(
    mut events: broadcast::Receiver<Event>,
    writer: mpsc::Sender<Event>,
) {
    loop {
        match events.recv().await {
            Ok(event) => {
                if !forward(&writer, event).await {
                    return;
                }
            },
            Err(RecvError::Lagged(missed)) => {
                tracing::error!(missed, "event store fell behind the event bus");
                if !forward(
                    &writer,
                    Event::new(LAGGED_KIND, json!({ "missed": missed })),
                )
                .await
                {
                    return;
                }
            },
            Err(RecvError::Closed) => return,
        }
    }
}

/// Sends `event` to the writer thread, returning `false` if it stopped.
async fn forward(
    writer: &mpsc::Sender<Event>,
    event: Event,
) -> bool {
    if writer.send(event).await.is_ok() {
        return true;
    }
    tracing::error!("event store writer stopped; events are no longer persisted");
    false
}

#[cfg(test)]
mod tests {
    use super::{LAGGED_KIND, StoreError, open};
    use crate::events::{Event, EventBus};
    use rusqlite::Connection;
    use serde_json::json;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    /// Polls the database until at least `expected` events are stored, then
    /// returns them in `seq` order.
    async fn wait_for_events(
        path: &Path,
        expected: usize,
    ) -> Vec<Event> {
        for _ in 0..500 {
            if let Ok(connection) = Connection::open(path) {
                let events = read_events(&connection);
                if events.len() >= expected {
                    return events;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("timed out waiting for {expected} persisted events");
    }

    /// Reads the stored `CloudEvents` envelopes in `seq` order.
    fn read_events(connection: &Connection) -> Vec<Event> {
        let mut statement = connection
            .prepare("SELECT event FROM events ORDER BY seq")
            .expect("select should compile");
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .expect("query should execute");
        rows.collect::<Result<Vec<_>, _>>()
            .expect("rows should read")
            .into_iter()
            .map(|envelope| serde_json::from_str(&envelope).expect("events should deserialize"))
            .collect()
    }

    #[tokio::test]
    async fn open_applies_the_initial_migration_idempotently() -> Result<(), StoreError> {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let path = dir.path().join("events.db");

        open(&path, &EventBus::default())?;
        open(&path, &EventBus::default())?;

        let connection = Connection::open(&path).expect("database should open");
        let version: u32 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("user_version should read");
        assert_eq!(version, 1);

        Ok(())
    }

    #[tokio::test]
    async fn open_persists_events_published_on_the_bus() -> Result<(), StoreError> {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let path = dir.path().join("events.db");
        let bus = EventBus::new(16);
        open(&path, &bus)?;

        let events: Vec<Event> = (0..3)
            .map(|index| Event::new(format!("test.event.{index}"), json!({ "index": index })))
            .collect();
        for event in &events {
            bus.publish(event.clone());
        }

        let stored = wait_for_events(&path, events.len()).await;

        assert_eq!(stored, events);

        Ok(())
    }

    #[tokio::test]
    async fn open_records_missed_events_as_a_lagged_marker() -> Result<(), StoreError> {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let path = dir.path().join("events.db");
        let bus = EventBus::new(1);
        open(&path, &bus)?;

        bus.publish(Event::new("test.first", json!({ "index": 0 })));
        bus.publish(Event::new("test.second", json!({ "index": 1 })));
        bus.publish(Event::new("test.third", json!({ "index": 2 })));

        let stored = wait_for_events(&path, 2).await;

        let kinds: Vec<&str> = stored.iter().map(|event| event.kind.as_str()).collect();
        assert_eq!(kinds, [LAGGED_KIND, "test.third"]);
        let marker = stored
            .iter()
            .find(|event| event.kind == LAGGED_KIND)
            .expect("the lagged marker should be persisted");
        assert_eq!(marker.data["missed"], 2);

        Ok(())
    }

    #[tokio::test]
    async fn open_rejects_newer_schemas() -> Result<(), rusqlite::Error> {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let path: PathBuf = dir.path().join("events.db");
        {
            let connection = Connection::open(&path)?;
            connection.pragma_update(None, "user_version", 99)?;
        }

        let result = open(&path, &EventBus::default());

        assert!(matches!(
            result,
            Err(StoreError::UnsupportedSchema {
                stored: 99,
                latest: 1
            })
        ));

        Ok(())
    }
}
