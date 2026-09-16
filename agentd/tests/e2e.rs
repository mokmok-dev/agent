//! End-to-end coverage of the choreography flow: WebSocket clients exchange
//! `CloudEvents` over the Unix domain socket, the server relays them through
//! the durable event log, and every event is appended to JSONL.
//!
//! The helpers below use `expect` and `panic` like the `#[cfg(test)]` modules
//! in `src` do; the workspace `allow-*-in-tests` clippy configuration cannot
//! see integration test files, so it is replicated here.

#![allow(clippy::expect_used, clippy::panic)]

use agentd::log::EventLog;
use agentd::server::router;
use agentd_events::Event;
use futures_util::SinkExt;
use futures_util::StreamExt;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::net::UnixStream;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;

/// Spawns a server on `socket` without signal handling.
fn spawn_server(
    socket: PathBuf,
    log: EventLog,
) -> tokio::task::JoinHandle<std::io::Result<()>> {
    tokio::spawn(async move {
        let listener = tokio::net::UnixListener::bind(socket)?;
        let () = axum::serve(listener, router(log)).await?;
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

/// Reads every event from the JSONL log in order.
fn read_log(path: &Path) -> Vec<Event> {
    std::fs::read_to_string(path)
        .expect("log should be readable")
        .lines()
        .map(|line| serde_json::from_str(line).expect("line should decode"))
        .collect()
}

#[tokio::test]
async fn choreographed_events_flow_from_websockets_into_the_log() {
    let dir = tempfile::tempdir().expect("tempdir should be created");
    let socket = dir.path().join("test.sock");
    let log_path = dir.path().join("events.jsonl");
    let log = EventLog::open(&log_path).expect("log should open");
    let server = spawn_server(socket.clone(), log);

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

    assert_eq!(read_log(&log_path), [submitted, completed]);

    server.abort();
}