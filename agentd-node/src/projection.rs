//! A SQLite-backed read model.
//!
//! [`SqliteProjection`] derives state from the event log by applying events in
//! [`Seq`] order. The JSONL event store is the source of truth; the SQLite file
//! is a projection that can always be rebuilt from the log, so it is safe to
//! delete when the schema or the reducer changes.
//!
//! The checkpoint of the last applied position lives in the same database and is
//! written in the **same transaction** as the reducer's state change: a crash
//! can never leave the checkpoint ahead of the state it describes, so a restart
//! never silently skips an event. Applying an event at or before the checkpoint
//! is a no-op, which makes replay idempotent.
//!
//! The reducer owns its schema and its state changes; this module only owns the
//! checkpoint and the transaction that keeps the two consistent.

use agentd_events::{LogEntry, Projection, Seq};
use rusqlite::{Connection, OptionalExtension, Transaction};
use std::path::Path;
use std::time::Duration;
use thiserror::Error;

/// How long a SQLite operation waits for a lock held by another connection
/// before failing. The node is the only writer, but a reader (for example a
/// query tool) may hold a lock briefly.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// The table holding the projection checkpoint. The single row is enforced by
/// the `CHECK (id = 1)` constraint.
const CHECKPOINT_SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS _agentd_checkpoint (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    applied_seq INTEGER NOT NULL
);";

/// A `rusqlite` error surfaced by a reducer that only performs SQL.
///
/// Implement [`SqliteReducer`] with `type Error = SqliteError` when nothing
/// beyond a database error can occur, and use a richer error type when the
/// domain needs one.
#[derive(Debug, Error)]
#[error(transparent)]
pub struct SqliteError(#[from] pub rusqlite::Error);

/// The domain logic of a projection: its schema and how one event changes its
/// state.
///
/// The reducer is stateless at the type level; its state lives in the SQLite
/// database. [`reduce`](SqliteReducer::reduce) runs inside the transaction that
/// also advances the checkpoint, so any SQL error rolls the whole event back.
pub trait SqliteReducer {
    /// The error returned when migrating or reducing fails.
    type Error: std::error::Error + From<rusqlite::Error> + 'static;

    /// Creates the reducer's tables if they do not exist.
    ///
    /// This runs on every open and must be idempotent.
    ///
    /// # Errors
    ///
    /// Returns an error if the schema cannot be created.
    fn migrate(conn: &Connection) -> Result<(), Self::Error>;

    /// Applies `entry` to the reducer's state inside `tx`.
    ///
    /// The checkpoint is written by [`SqliteProjection`] in the same
    /// transaction, so `reduce` must not touch `_agentd_checkpoint`.
    ///
    /// # Errors
    ///
    /// Returns an error to reject the event; the transaction is rolled back.
    fn reduce(
        tx: &Transaction<'_>,
        entry: &LogEntry,
    ) -> Result<(), Self::Error>;
}

/// A read model stored in one SQLite database.
///
/// The type parameter is the [`SqliteReducer`] that defines the schema and the
/// state transition.
#[derive(Debug)]
pub struct SqliteProjection<R> {
    conn: Connection,
    applied: Seq,
    reducer: std::marker::PhantomData<fn() -> R>,
}

impl<R> SqliteProjection<R>
where
    R: SqliteReducer,
{
    /// Opens (or creates) the projection at `path`, migrates the schema, and
    /// reads the checkpoint.
    ///
    /// A fresh database starts at position zero, so a later [`catch_up`] or node
    /// replay rebuilds the whole projection from the log.
    ///
    /// # Errors
    ///
    /// Returns an error if the database cannot be opened, migrated, or read.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, R::Error> {
        let conn = Connection::open(path)?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        conn.execute_batch(CHECKPOINT_SCHEMA)?;
        R::migrate(&conn)?;
        let applied = read_checkpoint(&conn)?;
        Ok(Self {
            conn,
            applied,
            reducer: std::marker::PhantomData,
        })
    }

    /// The position of the last event this projection applied or skipped.
    #[must_use]
    pub const fn applied_seq(&self) -> Seq {
        self.applied
    }

    /// Borrows the underlying connection for read queries.
    ///
    /// Writes must go through [`apply`](Self::apply) or [`skip`](Self::skip) so
    /// the checkpoint stays consistent with the state.
    #[must_use]
    pub const fn connection(&self) -> &Connection {
        &self.conn
    }

    /// Applies `recorded` and advances the checkpoint in one transaction.
    ///
    /// An event at or before the checkpoint is ignored, so replaying a range
    /// (for example after a reconnect) is idempotent.
    ///
    /// # Errors
    ///
    /// Returns [`SqliteReducer::Error`] if the reducer rejects the event; the
    /// state and checkpoint are rolled back together.
    ///
    /// This mirrors [`Projection::apply`], so it takes the entry by value and
    /// the projection is usable without importing the trait.
    #[allow(clippy::needless_pass_by_value)]
    pub fn apply(
        &mut self,
        recorded: LogEntry,
    ) -> Result<(), R::Error> {
        self.apply_entry(&recorded)
    }

    /// Advances the checkpoint to `seq` without changing reducer state.
    ///
    /// Used for events the node is not interested in: skipping them keeps the
    /// checkpoint current so a restart does not re-read them.
    ///
    /// # Errors
    ///
    /// Returns [`SqliteReducer::Error`] if the checkpoint cannot be written.
    pub fn skip(
        &mut self,
        seq: Seq,
    ) -> Result<(), R::Error> {
        if seq <= self.applied {
            return Ok(());
        }
        let tx = self.conn.transaction()?;
        write_checkpoint(&tx, seq)?;
        tx.commit()?;
        self.applied = seq;
        Ok(())
    }

    fn apply_entry(
        &mut self,
        recorded: &LogEntry,
    ) -> Result<(), R::Error> {
        if recorded.seq <= self.applied {
            return Ok(());
        }
        let tx = self.conn.transaction()?;
        R::reduce(&tx, recorded)?;
        write_checkpoint(&tx, recorded.seq)?;
        tx.commit()?;
        self.applied = recorded.seq;
        Ok(())
    }
}

impl<R> Projection for SqliteProjection<R>
where
    R: SqliteReducer,
{
    type Error = R::Error;

    fn applied_seq(&self) -> Seq {
        self.applied
    }

    fn apply(
        &mut self,
        recorded: LogEntry,
    ) -> Result<(), Self::Error> {
        self.apply_entry(&recorded)
    }
}

/// Reads the checkpoint, treating a missing row as position zero.
fn read_checkpoint(conn: &Connection) -> rusqlite::Result<Seq> {
    let value: Option<i64> = conn
        .query_row(
            "SELECT applied_seq FROM _agentd_checkpoint WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    value.map_or_else(|| Ok(0), seq_from_sql)
}

/// Writes the checkpoint in `tx`.
fn write_checkpoint(
    tx: &Transaction<'_>,
    seq: Seq,
) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO _agentd_checkpoint (id, applied_seq) VALUES (1, ?1) \
         ON CONFLICT(id) DO UPDATE SET applied_seq = excluded.applied_seq",
        [seq_to_sql(seq)?],
    )?;
    Ok(())
}

/// Converts a log position to a SQLite integer.
fn seq_to_sql(seq: Seq) -> rusqlite::Result<i64> {
    i64::try_from(seq)
        .map_err(|_| rusqlite::Error::ToSqlConversionFailure(Box::new(SeqOutOfRange(seq))))
}

/// Converts a stored integer back to a log position, rejecting negatives.
fn seq_from_sql(value: i64) -> rusqlite::Result<Seq> {
    u64::try_from(value).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(0, value))
}

/// The error used when a position does not fit the SQLite integer range.
#[derive(Debug, Error)]
#[error("log position {0} does not fit in a SQLite integer")]
struct SeqOutOfRange(Seq);

#[cfg(test)]
mod tests {
    use super::{SqliteError, SqliteProjection, SqliteReducer};
    use agentd_events::log::EventLog;
    use agentd_events::{Event, LogEntry, catch_up};
    use rusqlite::{Connection, Transaction};
    use serde_json::json;

    /// A projection counting events per type.
    struct EventCounts;

    impl SqliteReducer for EventCounts {
        type Error = SqliteError;

        fn migrate(conn: &Connection) -> Result<(), Self::Error> {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS event_counts (
                    type TEXT PRIMARY KEY,
                    count INTEGER NOT NULL
                );",
            )?;
            Ok(())
        }

        fn reduce(
            tx: &Transaction<'_>,
            entry: &LogEntry,
        ) -> Result<(), Self::Error> {
            tx.execute(
                "INSERT INTO event_counts (type, count) VALUES (?1, 1) \
                 ON CONFLICT(type) DO UPDATE SET count = count + 1",
                [&entry.event.r#type],
            )?;
            Ok(())
        }
    }

    fn entry(
        seq: u64,
        r#type: &str,
    ) -> LogEntry {
        LogEntry::new(seq, Event::new(r#type, json!({})))
    }

    fn count(
        projection: &SqliteProjection<EventCounts>,
        r#type: &str,
    ) -> i64 {
        projection
            .connection()
            .query_row(
                "SELECT count FROM event_counts WHERE type = ?1",
                [r#type],
                |row| row.get(0),
            )
            .expect("count should be readable")
    }

    #[test]
    fn apply_persists_state_and_checkpoint() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let path = dir.path().join("node.db");

        {
            let mut projection =
                SqliteProjection::<EventCounts>::open(&path).expect("open should succeed");
            projection
                .apply(entry(1, "a"))
                .expect("apply should succeed");
            projection
                .apply(entry(2, "a"))
                .expect("apply should succeed");
            projection
                .apply(entry(3, "b"))
                .expect("apply should succeed");
            assert_eq!(projection.applied_seq(), 3);
        }

        let projection =
            SqliteProjection::<EventCounts>::open(&path).expect("reopen should succeed");
        assert_eq!(projection.applied_seq(), 3);
        assert_eq!(count(&projection, "a"), 2);
        assert_eq!(count(&projection, "b"), 1);
    }

    #[test]
    fn applying_at_or_before_the_checkpoint_is_a_noop() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let mut projection = SqliteProjection::<EventCounts>::open(dir.path().join("node.db"))
            .expect("open should succeed");

        projection
            .apply(entry(1, "a"))
            .expect("apply should succeed");
        projection
            .apply(entry(1, "a"))
            .expect("replay should succeed");
        projection
            .apply(entry(2, "a"))
            .expect("apply should succeed");

        assert_eq!(projection.applied_seq(), 2);
        assert_eq!(count(&projection, "a"), 2);
    }

    #[test]
    fn skip_advances_the_checkpoint_without_counting() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let mut projection = SqliteProjection::<EventCounts>::open(dir.path().join("node.db"))
            .expect("open should succeed");

        projection
            .apply(entry(1, "a"))
            .expect("apply should succeed");
        projection.skip(2).expect("skip should succeed");
        projection
            .apply(entry(3, "b"))
            .expect("apply should succeed");

        assert_eq!(projection.applied_seq(), 3);
        assert_eq!(count(&projection, "a"), 1);
        assert_eq!(count(&projection, "b"), 1);
    }

    #[tokio::test]
    async fn catch_up_replays_the_log_through_the_projection() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let log = EventLog::open(dir.path().join("events.jsonl")).expect("log should open");
        for index in 0..2 {
            log.publish(Event::new("test.a", json!({ "index": index })))
                .await
                .expect("publish should succeed");
        }
        log.publish(Event::new("test.b", json!({})))
            .await
            .expect("publish should succeed");

        let mut projection = SqliteProjection::<EventCounts>::open(dir.path().join("node.db"))
            .expect("open should succeed");
        catch_up(&log, &mut projection).expect("catch up should succeed");

        assert_eq!(projection.applied_seq(), 3);
        assert_eq!(count(&projection, "test.a"), 2);
        assert_eq!(count(&projection, "test.b"), 1);
    }
}
