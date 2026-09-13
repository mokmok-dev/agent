//! End-to-end coverage of the choreography flow: WebSocket clients exchange
//! `CloudEvents` over the Unix domain socket, the server relays them through
//! the event bus, and the event store persists everything in SQLite.
//!
//! The helpers below use `expect` and `panic` like the `#[cfg(test)]` modules
//! in `src` do; the workspace `allow-*-in-tests` clippy configuration cannot
//! see integration test files, so it is replicated here.

#![allow(clippy::expect_used, clippy::panic)]

use agentd::eventstore::open;
use agentd::server::router;
use agentd_events::{Event, EventBus};
use futures_util::SinkExt;
use futures_util::StreamExt;
use rusqlite::Connection;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::net::UnixStream;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;

/// A stored event: the denormalized context attributes plus the `CloudEvents`
/// envelope.
struct Row {
    id: String,
    source: String,
    specversion: String,
    kind: String,
    time: Option<String>,
    envelope: Event,
}

/// Spawns a server on `socket` without signal handling.
fn spawn_server(
    socket: PathBuf,
    bus: EventBus,
) -> tokio::task::JoinHandle<std::io::Result<()>> {
    tokio::spawn(async move {
        let listener = tokio::net::UnixListener::bind(socket)?;
        let () = axum::serve(listener, router(bus)).await?;
        Ok(())
    })
}

/// Connects a WebSocket client to `/events`, retrying while the server starts
/// up.
async fn connect(socket: &Path) -> WebSocketStream<UnixStream> {
    for _ in 0..100 {
        if let Ok(stream) = UnixStream::connect(socket).await {
            let (ws, _) = tokio_tungstenite::client_async("ws://localhost/events", stream)
                .await
                .expect("handshake should succeed");
            return ws;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("could not connect to {}", socket.display());
}

/// Receives the next event from `client`, failing the test on timeout,
/// disconnects, or malformed messages.
async fn recv_event(client: &mut WebSocketStream<UnixStream>) -> Event {
    let message = tokio::time::timeout(Duration::from_secs(5), client.next())
        .await
        .expect("timed out waiting for event")
        .expect("stream should not end")
        .expect("read should succeed");
    let Message::Text(text) = message else {
        panic!("expected a text message, got {message:?}");
    };
    serde_json::from_str(&text).expect("expected a valid event")
}

/// Polls the database until at least `expected` events are stored, then
/// returns them in `seq` order.
async fn wait_for_rows(
    path: &Path,
    expected: usize,
) -> Vec<Row> {
    for _ in 0..500 {
        if let Ok(connection) = Connection::open(path) {
            let rows = read_rows(&connection);
            if rows.len() >= expected {
                return rows;
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {expected} persisted events");
}

/// Reads the stored events with their denormalized attributes in `seq` order.
fn read_rows(connection: &Connection) -> Vec<Row> {
    let mut statement = connection
        .prepare(
            "SELECT id, source, specversion, type, time, event
             FROM events
             ORDER BY seq",
        )
        .expect("select should compile");
    let rows = statement
        .query_map([], |row| {
            Ok(Row {
                id: row.get(0)?,
                source: row.get(1)?,
                specversion: row.get(2)?,
                kind: row.get(3)?,
                time: row.get(4)?,
                envelope: serde_json::from_str(&row.get::<_, String>(5)?)
                    .expect("events should deserialize"),
            })
        })
        .expect("query should execute");
    rows.collect::<Result<Vec<_>, _>>()
        .expect("rows should read")
}

#[tokio::test]
async fn choreographed_events_flow_from_websockets_into_sqlite() {
    let dir = tempfile::tempdir().expect("tempdir should be created");
    let socket = dir.path().join("test.sock");
    let db_path = dir.path().join("events.db");
    let bus = EventBus::new(16);
    open(&db_path, &bus).expect("event store should open");
    let server = spawn_server(socket.clone(), bus);

    let mut producer = connect(&socket).await;
    let mut worker = connect(&socket).await;

    let submitted = Event::new("task.submitted", json!({ "task": "demo" }));
    producer
        .send(Message::from(
            serde_json::to_string(&submitted).expect("event should serialize"),
        ))
        .await
        .expect("send should succeed");

    let completed = {
        let relayed = recv_event(&mut worker).await;
        assert_eq!(relayed, submitted);

        let completed = Event::new("task.completed", json!({ "task": "demo" }));
        worker
            .send(Message::from(
                serde_json::to_string(&completed).expect("event should serialize"),
            ))
            .await
            .expect("send should succeed");
        completed
    };

    assert_eq!(recv_event(&mut producer).await, submitted);
    assert_eq!(recv_event(&mut producer).await, completed);

    let rows = wait_for_rows(&db_path, 2).await;
    let events = [submitted, completed];
    assert_eq!(rows.len(), events.len());
    for (event, row) in events.iter().zip(&rows) {
        assert_eq!(row.id, event.id);
        assert_eq!(row.source, event.source);
        assert_eq!(row.specversion, event.specversion);
        assert_eq!(row.kind, event.kind);
        assert_eq!(row.time, event.time);
        assert_eq!(row.envelope, *event);
    }

    server.abort();
}
