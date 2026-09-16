//! End-to-end test: a node projects the daemon's event log into SQLite and
//! resumes from its checkpoint across a restart.

use agentd::server;
use agentd_events::{Event, EventLog, LogEntry, Seq};
use agentd_node::{Node, SqliteError, SqliteProjection, SqliteReducer, TypePrefixes, WsClient};
use rusqlite::{Connection, Transaction};
use serde_json::json;
use std::path::Path;
use std::time::Duration;
use tokio::net::UnixListener;
use tokio::sync::watch;

/// Counts events per `type`.
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

/// Polls the projection until its checkpoint reaches `expected`.
async fn wait_for_seq(
    db: &Path,
    expected: Seq,
) -> SqliteProjection<EventCounts> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(projection) = SqliteProjection::<EventCounts>::open(db)
            && projection.applied_seq() >= expected
        {
            return projection;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for position {expected}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Reads the projected count for `r#type`, or zero when it was skipped.
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
        .unwrap_or(0)
}

/// Spawns a node that projects into `db` and returns its shutdown handle.
#[allow(clippy::expect_used)]
fn spawn_node(
    socket: &Path,
    db: &Path,
) -> (
    watch::Sender<bool>,
    tokio::task::JoinHandle<Result<(), agentd_node::NodeError<SqliteError>>>,
) {
    let projection = SqliteProjection::<EventCounts>::open(db).expect("projection should open");
    let mut node = Node::new(
        socket,
        projection,
        TypePrefixes::new(["test."]),
        "urn:mokmokd:session:test",
    );
    let (sender, receiver) = watch::channel(false);
    let handle = tokio::spawn(async move { node.run(receiver).await });
    (sender, handle)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_projects_from_the_log_and_resumes_from_its_checkpoint() {
    let dir = tempfile::tempdir().expect("tempdir should be created");
    let socket = dir.path().join("test.sock");
    let db = dir.path().join("node.db");
    let log = EventLog::open(dir.path().join("events.jsonl")).expect("log should open");

    let listener = UnixListener::bind(&socket).expect("listener should bind");
    let server_log = log.clone();
    let server = tokio::spawn(async move {
        axum::serve(listener, server::router(server_log))
            .await
            .expect("server should run");
    });

    for index in 0..3 {
        log.publish(Event::new("test.event", json!({ "index": index })))
            .await
            .expect("publish should succeed");
    }

    let (shutdown, node) = spawn_node(&socket, &db);

    let projection = wait_for_seq(&db, 3).await;
    assert_eq!(count(&projection, "test.event"), 3);
    drop(projection);

    // An event outside the filter advances the checkpoint but is not projected.
    log.publish(Event::new("other.event", json!({})))
        .await
        .expect("publish should succeed");
    let projection = wait_for_seq(&db, 4).await;
    assert_eq!(count(&projection, "test.event"), 3);
    assert_eq!(count(&projection, "other.event"), 0);
    drop(projection);

    shutdown.send(true).expect("shutdown should be sent");
    node.await
        .expect("node should join")
        .expect("node should stop cleanly");

    // The checkpoint and state survive the restart.
    let projection = SqliteProjection::<EventCounts>::open(&db).expect("projection should reopen");
    assert_eq!(projection.applied_seq(), 4);
    assert_eq!(count(&projection, "test.event"), 3);
    drop(projection);

    // A new node resumes after the checkpoint: it neither replays from zero nor
    // applies the skipped event.
    let (shutdown, node) = spawn_node(&socket, &db);
    log.publish(Event::new("test.event", json!({ "index": 3 })))
        .await
        .expect("publish should succeed");
    let projection = wait_for_seq(&db, 5).await;
    assert_eq!(count(&projection, "test.event"), 4);
    assert_eq!(count(&projection, "other.event"), 0);
    drop(projection);

    shutdown.send(true).expect("shutdown should be sent");
    node.await
        .expect("node should join")
        .expect("node should stop cleanly");

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ws_client_publishes_an_event_the_daemon_broadcasts_back() {
    let dir = tempfile::tempdir().expect("tempdir should be created");
    let socket = dir.path().join("test.sock");
    let log = EventLog::open(dir.path().join("events.jsonl")).expect("log should open");

    let listener = UnixListener::bind(&socket).expect("listener should bind");
    let server_log = log.clone();
    let server = tokio::spawn(async move {
        axum::serve(listener, server::router(server_log))
            .await
            .expect("server should run");
    });

    let mut client = WsClient::connect(&socket, None)
        .await
        .expect("client should connect");
    let event = Event::new("test.sent", json!({ "n": 1 }));
    client.send(&event).await.expect("send should succeed");

    let wire = tokio::time::timeout(Duration::from_secs(5), client.next())
        .await
        .expect("timed out waiting for the broadcast")
        .expect("read should succeed")
        .expect("the connection should stay open");

    assert_eq!(wire.seq, Some(1));
    assert_eq!(wire.event, event);

    server.abort();
}
