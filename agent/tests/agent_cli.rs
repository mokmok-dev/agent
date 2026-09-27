//! End-to-end test of the `agent` binary: start the daemon, connect over its
//! UDS, and drive publish/subscribe through it.
//!
//! This exercises the whole stack the way an operator runs it: `agent` binds the
//! socket, opens the bus, and serves the wire protocol.
#![expect(
    clippy::expect_used,
    clippy::panic,
    reason = "integration test code may panic when a fixture fails"
)]

use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::net::UnixStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderValue, header};

/// A running `agent` daemon, killed on drop.
struct Daemon {
    child: Child,
    /// The directory to remove on drop; `None` once the daemon is stopped and
    /// its data is kept for a restart.
    cleanup: Option<PathBuf>,
    socket: PathBuf,
}

impl Daemon {
    /// Kill the daemon but keep its directory, so another daemon can reopen it.
    fn stop_keeping_data(self) -> PathBuf {
        let mut this = self;
        let _ = this.child.kill();
        let _ = this.child.wait();
        this.cleanup.take().expect("not already stopped")
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(dir) = &self.cleanup {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// Start `agent` on a fresh socket with this process's UID permitted.
fn start(tag: &str) -> Daemon {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    let root =
        std::env::temp_dir().join(format!("agent-e2e-{tag}-{}-{unique}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let socket = root.join("bus.sock");
    start_over(root, socket)
}

/// Start `agent` over an existing root, for a restart test.
fn start_over(
    root: PathBuf,
    socket: PathBuf,
) -> Daemon {
    let uid = std::fs::metadata(std::env::temp_dir())
        .expect("the temp dir has metadata")
        .uid();

    let child = Command::new(env!("CARGO_BIN_EXE_agent"))
        .arg("--dir")
        .arg(root.join("data"))
        .arg("--socket")
        .arg(&socket)
        .arg("--allow-uid")
        .arg(uid.to_string())
        .arg("--log-format")
        .arg("json")
        .spawn()
        .expect("the agent binary starts");

    Daemon {
        child,
        cleanup: Some(root),
        socket,
    }
}

/// Wait until the daemon actually accepts a connection.
///
/// Checking for the socket file is not enough: a killed daemon leaves a stale
/// socket file that still exists, and connecting to it fails until the new
/// daemon reclaims the path. Probe-connecting is the honest readiness signal and
/// also exercises the stale-socket reclaim.
async fn wait_for_socket(daemon: &Daemon) {
    for _ in 0..100 {
        if UnixStream::connect(&daemon.socket).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the socket never accepted at {}", daemon.socket.display());
}

/// A connected WebSocket client for the test.
type WebSocket = tokio_tungstenite::WebSocketStream<UnixStream>;

/// Connect a WebSocket client, offering the bus subprotocol.
async fn connect(socket: &std::path::Path) -> WebSocket {
    let stream = UnixStream::connect(socket).await.expect("connects");
    let mut request = "ws://localhost/".into_client_request().expect("request");
    request.headers_mut().insert(
        header::SEC_WEBSOCKET_PROTOCOL,
        HeaderValue::from_static("agent.eventbus.v1"),
    );
    let (websocket, _) = tokio_tungstenite::client_async(request, stream)
        .await
        .expect("handshakes");
    websocket
}

/// Send a text frame.
async fn send(
    websocket: &mut WebSocket,
    text: &str,
) {
    websocket
        .send(Message::text(text.to_owned()))
        .await
        .expect("sends");
}

/// Receive the next text frame as JSON.
async fn recv(websocket: &mut WebSocket) -> serde_json::Value {
    loop {
        let message = tokio::time::timeout(Duration::from_secs(5), websocket.next())
            .await
            .expect("a frame arrives in time")
            .expect("a frame")
            .expect("a valid frame");
        if message.is_text() {
            return serde_json::from_str(message.to_text().expect("text")).expect("json");
        }
    }
}

#[tokio::test]
async fn the_daemon_serves_a_publish_and_a_subscribe() {
    let daemon = start("serve");
    wait_for_socket(&daemon).await;
    let mut client = connect(&daemon.socket).await;

    // Publish an event through the daemon.
    let request = serde_json::json!({
        "type": "publish",
        "id": "req-1",
        "event": {
            "specversion": "1.0",
            "type": "agent.task.started",
            "data": {"task_id": "t-1"},
        },
    });
    send(&mut client, &request.to_string()).await;
    let reply = recv(&mut client).await;
    assert_eq!(reply["type"], "published");
    assert_eq!(reply["seq"], 0);

    // Subscribe and read the stored envelope back.
    send(
        &mut client,
        r#"{"type":"subscribe","subscriber_id":"audit","from_seq":0}"#,
    )
    .await;
    assert_eq!(recv(&mut client).await["type"], "subscribed");
    let event = recv(&mut client).await;
    assert_eq!(event["type"], "agent.task.started");
    assert_eq!(event["data"]["task_id"], "t-1");
    assert_eq!(event["source"], "agent://eventbus");
}

#[tokio::test]
async fn the_daemon_persists_across_a_restart() {
    let first = start("restart");
    wait_for_socket(&first).await;
    let mut client = connect(&first.socket).await;
    let request = serde_json::json!({
        "type": "publish",
        "id": "req-1",
        "event": {"specversion": "1.0", "type": "agent.task.started"},
    });
    send(&mut client, &request.to_string()).await;
    assert_eq!(recv(&mut client).await["type"], "published");

    // Stop the daemon, keeping its data directory, and start a new one over it.
    drop(client);
    let root = first.stop_keeping_data();

    // The socket file remains after the kill; a new daemon reclaims it.
    let socket = root.join("bus.sock");
    let daemon = start_over(root, socket);
    wait_for_socket(&daemon).await;

    // The event from before the restart is still in the log.
    let mut client = connect(&daemon.socket).await;
    send(
        &mut client,
        r#"{"type":"subscribe","subscriber_id":"audit","from_seq":0}"#,
    )
    .await;
    assert_eq!(recv(&mut client).await["type"], "subscribed");
    let event = recv(&mut client).await;
    assert_eq!(event["type"], "agent.task.started");
    assert_eq!(event["sequence"], "00000000000000000000");
}

#[tokio::test]
async fn the_daemon_refuses_a_privileged_event_without_an_authority_socket() {
    let daemon = start("no-authority");
    wait_for_socket(&daemon).await;
    let mut client = connect(&daemon.socket).await;

    let request = serde_json::json!({
        "type": "publish",
        "id": "req-1",
        "event": {
            "specversion": "1.0",
            "type": "agent.sandbox.egress.granted",
        },
    });
    send(&mut client, &request.to_string()).await;
    let reply = recv(&mut client).await;
    assert_eq!(reply["type"], "error");
    assert_eq!(reply["code"], "forbidden");
}
