//! End-to-end coverage of the choreography flow: authenticated WebSocket
//! clients exchange `CloudEvents` over the Unix domain socket, the server
//! relays them through the durable event log, and every event is appended to
//! JSONL with daemon-owned provenance.
//!
//! The helpers below use `expect` and `panic` like the `#[cfg(test)]` modules
//! in `src` do; the workspace `allow-*-in-tests` clippy configuration cannot
//! see integration test files, so it is replicated here.

#![allow(clippy::expect_used, clippy::panic)]

use agentd::auth::{Claim, Principal, Token, TokenStore};
use agentd::server::router;
use agentd_events::{Event, EventLog, Seq};
use agentd_inference::FakeProvider;
use futures_util::SinkExt;
use futures_util::StreamExt;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UnixStream;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

/// The read-and-publish token used by the tests.
const WRITER_TOKEN: &str = "writer-secret";

/// A token store granting read and publish.
fn tokens() -> TokenStore {
    TokenStore::new(vec![Token {
        secret: String::from(WRITER_TOKEN),
        principal: Principal::new("urn:test:writer", [Claim::Read, Claim::Publish]),
    }])
}

/// Spawns a server on `socket` without signal handling.
fn spawn_server(
    socket: PathBuf,
    log: EventLog,
) -> tokio::task::JoinHandle<std::io::Result<()>> {
    tokio::spawn(async move {
        let listener = tokio::net::UnixListener::bind(socket)?;
        let () = axum::serve(
            listener,
            router(log, tokens(), Arc::new(FakeProvider::default())),
        )
        .await?;
        Ok(())
    })
}

/// Connects a WebSocket client to `url`, retrying while the server starts up.
async fn connect_to(
    url: &str,
    socket: &Path,
) -> WebSocketStream<UnixStream> {
    for _ in 0..100 {
        if let Ok(stream) = UnixStream::connect(socket).await {
            let mut request = url.into_client_request().expect("client request");
            request.headers_mut().insert(
                "authorization",
                format!("Bearer {WRITER_TOKEN}").parse().expect("header"),
            );
            let (ws, _) = tokio_tungstenite::client_async(request, stream)
                .await
                .expect("handshake should succeed");
            return ws;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("could not connect to {}", socket.display());
}

/// Connects a WebSocket client to `/events` without a resume position.
async fn connect(socket: &Path) -> WebSocketStream<UnixStream> {
    connect_to("ws://localhost/events", socket).await
}

/// Connects a WebSocket client to `/events` resuming from `from`.
async fn connect_from(
    socket: &Path,
    from: Seq,
) -> WebSocketStream<UnixStream> {
    connect_to(&format!("ws://localhost/events?from={from}"), socket).await
}

/// The wire envelope: an event paired with its log position.
#[derive(serde::Deserialize)]
struct WireEvent {
    seq: Option<Seq>,
    event: Event,
}

/// Receives the next event's position and value from `client`, failing the test
/// on timeout, disconnect, or malformed messages.
async fn recv_wire(client: &mut WebSocketStream<UnixStream>) -> (Option<Seq>, Event) {
    let message = tokio::time::timeout(Duration::from_secs(5), client.next())
        .await
        .expect("timed out waiting for event")
        .expect("stream should not end")
        .expect("read should succeed");
    let Message::Text(text) = message else {
        panic!("expected a text message, got {message:?}");
    };
    let wire: WireEvent = serde_json::from_str(&text).expect("expected a valid wire message");
    (wire.seq, wire.event)
}

/// Sends `event` as an inbound `CloudEvents` message.
async fn send_event(
    client: &mut WebSocketStream<UnixStream>,
    event: &Event,
) {
    client
        .send(Message::from(
            serde_json::to_string(event).expect("event should serialize"),
        ))
        .await
        .expect("send should succeed");
}

/// Asserts `received` is `sent` with the daemon-owned provenance overwritten.
fn assert_attributed(
    received: &Event,
    sent: &Event,
) {
    assert_eq!(received.id, sent.id);
    assert_eq!(received.r#type, sent.r#type);
    assert_eq!(received.data, sent.data);
    assert_eq!(received.source, "urn:test:writer");
    assert!(received.time.is_some());
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
    send_event(&mut producer, &submitted).await;

    let completed = {
        let (seq, relayed) = recv_wire(&mut worker).await;
        assert_eq!(seq, Some(1));
        assert_attributed(&relayed, &submitted);

        let completed = Event::new("task.completed", json!({ "task": "demo" }));
        send_event(&mut worker, &completed).await;
        completed
    };

    let (seq, first) = recv_wire(&mut producer).await;
    assert_eq!(seq, Some(1));
    assert_attributed(&first, &submitted);
    let (seq, second) = recv_wire(&mut producer).await;
    assert_eq!(seq, Some(2));
    assert_attributed(&second, &completed);

    let logged = read_log(&log_path);
    assert_eq!(logged.len(), 2);
    assert_attributed(&logged[0], &submitted);
    assert_attributed(&logged[1], &completed);

    server.abort();
}

#[tokio::test]
async fn a_reconnecting_consumer_resumes_from_its_cursor() {
    let dir = tempfile::tempdir().expect("tempdir should be created");
    let socket = dir.path().join("test.sock");
    let log_path = dir.path().join("events.jsonl");
    let log = EventLog::open(&log_path).expect("log should open");
    let server = spawn_server(socket.clone(), log.clone());

    // A producer publishes while the consumer is disconnected.
    let mut producer = connect(&socket).await;
    let first = Event::new("task.submitted", json!({ "task": "demo" }));
    let second = Event::new("task.started", json!({ "task": "demo" }));
    send_event(&mut producer, &first).await;
    let (seq, received) = recv_wire(&mut producer).await;
    assert_eq!(seq, Some(1));
    assert_attributed(&received, &first);
    send_event(&mut producer, &second).await;
    let (seq, received) = recv_wire(&mut producer).await;
    assert_eq!(seq, Some(2));
    assert_attributed(&received, &second);
    drop(producer);

    // The consumer reconnects from the position after the first event and sees
    // the second from history, then the third live.
    let mut consumer = connect_from(&socket, 2).await;
    let (seq, received) = recv_wire(&mut consumer).await;
    assert_eq!(seq, Some(2));
    assert_attributed(&received, &second);

    let third = Event::new("task.completed", json!({ "task": "demo" }));
    log.publish(third.clone())
        .await
        .expect("publish should succeed");
    let (seq, received) = recv_wire(&mut consumer).await;
    assert_eq!(seq, Some(3));
    assert_eq!(received.id, third.id);
    assert_eq!(received.source, agentd_events::DAEMON_SOURCE);

    server.abort();
}
