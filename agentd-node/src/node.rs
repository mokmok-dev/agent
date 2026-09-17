//! The node run loop: connect, replay, select, apply, repeat.
//!
//! A node is a long-lived process that consumes the daemon's event stream and
//! maintains a projection. It resumes from its checkpoint, applies the events it
//! is interested in, and advances past the rest, reconnecting with backoff when
//! the daemon is unavailable.

use crate::client::WsClient;
use crate::filter::Interest;
use crate::projection::{SqliteProjection, SqliteReducer};
use agentd_events::{LogEntry, Seq, WireMessage};
use secrecy::{ExposeSecret as _, SecretString};
use std::path::PathBuf;
use std::time::Duration;
use thiserror::Error;
use tokio::sync::watch;

/// The delay before the first reconnect attempt; doubled on each failure.
const INITIAL_BACKOFF: Duration = Duration::from_millis(100);

/// The longest delay between reconnect attempts.
const MAX_BACKOFF: Duration = Duration::from_secs(5);

/// The `CloudEvents` `type` of the daemon's out-of-range resume notice.
const RESUME_OUT_OF_RANGE: &str = "error.resume_out_of_range";

/// Errors returned by [`Node::run`].
///
/// A connection failure is not one of these: the node reconnects. A projection
/// failure or a rejected resume position is fatal, because continuing would
/// leave the node and the log permanently inconsistent.
#[derive(Debug, Error)]
pub enum NodeError<E>
where
    E: std::error::Error + 'static,
{
    /// The projection could not apply or skip an event.
    #[error("the projection failed: {0}")]
    Projection(#[source] E),
    /// The daemon rejected the node's resume position because it is beyond the
    /// log tail, so the projection is ahead of the log. This happens when the
    /// daemon starts with a fresh log (or a different log file); rebuild the
    /// projection with `--rebuild`.
    #[error(
        "the daemon rejected the resume position {position} beyond tail {tail}; \
         rebuild the projection"
    )]
    ResumeOutOfRange {
        /// The position the node tried to resume from.
        position: Seq,
        /// The daemon's last committed position.
        tail: Seq,
    },
}

/// A node that keeps a SQLite projection in sync with the event log.
#[derive(Debug)]
pub struct Node<F, R>
where
    R: SqliteReducer,
{
    socket: PathBuf,
    source: String,
    token: SecretString,
    interest: F,
    projection: SqliteProjection<R>,
}

impl<F, R> Node<F, R>
where
    F: Interest,
    R: SqliteReducer,
{
    /// Creates a node for `socket` with the given projection, interest, and
    /// stable `CloudEvents` `source` identity, authenticating with `token`.
    ///
    /// `token` accepts a `&str` or a `String`; a `&String` must be written
    /// `token.as_str()`.
    #[must_use]
    pub fn new(
        socket: impl Into<PathBuf>,
        projection: SqliteProjection<R>,
        interest: F,
        source: impl Into<String>,
        token: impl Into<SecretString>,
    ) -> Self {
        Self {
            socket: socket.into(),
            source: source.into(),
            token: token.into(),
            interest,
            projection,
        }
    }

    /// The node's `CloudEvents` `source` identity.
    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }

    /// The node's projection.
    #[must_use]
    pub const fn projection(&self) -> &SqliteProjection<R> {
        &self.projection
    }

    /// The position of the last event the node applied or skipped.
    #[must_use]
    pub const fn applied_seq(&self) -> Seq {
        self.projection.applied_seq()
    }

    /// Runs until `shutdown` becomes `true`, reconnecting with backoff while the
    /// daemon is unavailable.
    ///
    /// Each connection resumes from the checkpoint, so processing is
    /// at-least-once and idempotent: an event that was applied before a
    /// disconnect is not applied twice on the next connection.
    ///
    /// # Errors
    ///
    /// Returns [`NodeError::Projection`] if an event cannot be applied, which is
    /// fatal because the node can no longer trust its state.
    pub async fn run(
        &mut self,
        mut shutdown: watch::Receiver<bool>,
    ) -> Result<(), NodeError<R::Error>> {
        let mut backoff = INITIAL_BACKOFF;
        loop {
            if *shutdown.borrow() {
                return Ok(());
            }

            let from = self.projection.applied_seq().saturating_add(1);
            match WsClient::connect(&self.socket, Some(from), self.token.expose_secret()).await {
                Ok(mut client) => {
                    let received = self.session(&mut client, &mut shutdown).await?;
                    if received {
                        backoff = INITIAL_BACKOFF;
                    }
                    if *shutdown.borrow() {
                        return Ok(());
                    }
                },
                Err(error) => {
                    tracing::warn!(
                        %error,
                        socket = %self.socket.display(),
                        "failed to connect to the daemon",
                    );
                },
            }

            tokio::select! {
                () = tokio::time::sleep(backoff) => {},
                _ = shutdown.changed() => return Ok(()),
            }
            backoff = backoff.saturating_mul(2).min(MAX_BACKOFF);
        }
    }

    /// Processes one connection until it closes or `shutdown` fires, returning
    /// whether any message was received (used to decide whether to reset the
    /// reconnect backoff).
    async fn session(
        &mut self,
        client: &mut WsClient,
        shutdown: &mut watch::Receiver<bool>,
    ) -> Result<bool, NodeError<R::Error>> {
        let mut received = false;
        loop {
            tokio::select! {
                _ = shutdown.changed() => return Ok(received),
                message = client.next() => {
                    match message {
                        Ok(Some(wire)) => {
                            received = true;
                            self.handle(wire)?;
                        },
                        Ok(None) => return Ok(received),
                        Err(error) => {
                            tracing::warn!(%error, "node connection failed");
                            return Ok(received);
                        },
                    }
                },
            }
        }
    }

    /// Applies or skips one message, advancing the checkpoint either way.
    fn handle(
        &mut self,
        wire: WireMessage,
    ) -> Result<(), NodeError<R::Error>> {
        let Some(seq) = wire.seq else {
            return Self::handle_notice(&wire);
        };
        if seq <= self.projection.applied_seq() {
            return Ok(());
        }
        if self.interest.interested(&wire.event) {
            self.projection
                .apply(LogEntry::new(seq, wire.event))
                .map_err(NodeError::Projection)?;
        } else {
            self.projection.skip(seq).map_err(NodeError::Projection)?;
        }
        Ok(())
    }

    /// Handles a transient daemon notice, which never carries a position.
    ///
    /// A rejected resume position is fatal: the projection is ahead of the log,
    /// so reconnecting would repeat the rejection forever.
    fn handle_notice(wire: &WireMessage) -> Result<(), NodeError<R::Error>> {
        if wire.event.r#type == RESUME_OUT_OF_RANGE {
            return Err(NodeError::ResumeOutOfRange {
                position: json_position(&wire.event.data, "position"),
                tail: json_position(&wire.event.data, "tail"),
            });
        }
        tracing::debug!(r#type = %wire.event.r#type, "received a daemon notice");
        Ok(())
    }
}

/// Reads a log position from a notice's `data`, defaulting to zero when absent.
fn json_position(
    data: &serde_json::Value,
    field: &str,
) -> Seq {
    data.get(field)
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::{Node, NodeError};
    use crate::filter::TypePrefixes;
    use crate::projection::{SqliteError, SqliteProjection, SqliteReducer};
    use agentd_events::{Event, LogEntry};
    use rusqlite::{Connection, Transaction};
    use serde_json::json;
    use tokio::sync::watch;

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

    fn node(projection: SqliteProjection<EventCounts>) -> Node<TypePrefixes, EventCounts> {
        Node::new(
            "/tmp/agentd-node-test.sock",
            projection,
            TypePrefixes::new(["test."]),
            "urn:test:node",
            "test-token",
        )
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

    /// Returns the number of rows for `type`, which is zero when it was skipped.
    fn count_rows(
        projection: &SqliteProjection<EventCounts>,
        r#type: &str,
    ) -> i64 {
        projection
            .connection()
            .query_row(
                "SELECT count(*) FROM event_counts WHERE type = ?1",
                [r#type],
                |row| row.get(0),
            )
            .expect("row count should be readable")
    }

    #[test]
    fn handle_applies_interested_and_skips_the_rest() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let projection = SqliteProjection::<EventCounts>::open(dir.path().join("node.db"))
            .expect("open should succeed");
        let mut node = node(projection);

        node.handle(agentd_events::WireMessage {
            seq: Some(1),
            event: Event::new("test.keep", json!({})),
        })
        .expect("apply should succeed");
        node.handle(agentd_events::WireMessage {
            seq: Some(2),
            event: Event::new("other.drop", json!({})),
        })
        .expect("skip should succeed");
        node.handle(agentd_events::WireMessage {
            seq: None,
            event: Event::new("error.lagged", json!({})),
        })
        .expect("notice should be ignored");

        assert_eq!(node.applied_seq(), 2);
        assert_eq!(count(node.projection(), "test.keep"), 1);
        assert_eq!(count_rows(node.projection(), "other.drop"), 0);
    }

    #[test]
    fn a_rejected_resume_position_is_fatal() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let projection = SqliteProjection::<EventCounts>::open(dir.path().join("node.db"))
            .expect("open should succeed");
        let mut node = node(projection);

        let error = node
            .handle(agentd_events::WireMessage {
                seq: None,
                event: Event::new(
                    "error.resume_out_of_range",
                    json!({ "position": 5, "tail": 2 }),
                ),
            })
            .expect_err("an out-of-range resume position should stop the node");

        assert!(matches!(
            error,
            NodeError::ResumeOutOfRange {
                position: 5,
                tail: 2
            }
        ));
    }

    #[tokio::test]
    async fn run_stops_when_shutdown_is_set() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let projection = SqliteProjection::<EventCounts>::open(dir.path().join("node.db"))
            .expect("open should succeed");
        let mut node = node(projection);
        let (sender, receiver) = watch::channel(false);

        sender.send(true).expect("send should succeed");
        let result: Result<(), NodeError<SqliteError>> = node.run(receiver).await;

        assert!(result.is_ok());
        assert_eq!(node.applied_seq(), 0);
    }
}
