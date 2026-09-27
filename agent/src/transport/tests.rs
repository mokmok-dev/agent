//! Tests for the transport: socket permissions, stale-socket detection, the
//! subprotocol handshake, and peer credential capture.
//!
//! These drive the async listener on a real runtime, so the module is `async`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use tokio::net::UnixStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use super::*;

/// How long an `accept` that should succeed may take before the test fails.
/// Without this a regression in the accept loop hangs the suite instead of
/// failing it.
const ACCEPT_TIMEOUT: Duration = Duration::from_secs(10);

/// A unique socket path under the system temp dir, removed on drop.
struct SocketDir(PathBuf);

impl SocketDir {
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agent-transport-{tag}-{}-{unique}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        Self(dir)
    }

    fn socket(&self) -> PathBuf {
        self.0.join("bus.sock")
    }
}

impl Drop for SocketDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The mode bits of `path`.
fn mode(path: &Path) -> u32 {
    std::fs::metadata(path)
        .expect("metadata")
        .permissions()
        .mode()
        & 0o777
}

/// Accept one connection, failing rather than hanging if none arrives.
async fn accept_bounded(listener: &Listener) -> Connection {
    tokio::time::timeout(ACCEPT_TIMEOUT, listener.accept())
        .await
        .expect("accept did not complete in time")
        .expect("accepts")
}

/// Run the subprotocol callback against a request carrying `offered`.
#[expect(
    clippy::result_large_err,
    reason = "the tungstenite Callback contract returns the full response as the error"
)]
fn negotiate(offered: Option<&'static str>) -> Result<Response, ErrorResponse> {
    let mut request = Request::new(());
    if let Some(value) = offered {
        request.headers_mut().insert(
            header::SEC_WEBSOCKET_PROTOCOL,
            HeaderValue::from_static(value),
        );
    }
    ProtocolCallback.on_request(&request, Response::new(()))
}

#[test]
fn the_callback_echoes_a_supported_subprotocol() {
    let response = negotiate(Some(SUBPROTOCOL)).expect("supported is accepted");
    assert_eq!(
        response
            .headers()
            .get(header::SEC_WEBSOCKET_PROTOCOL)
            .and_then(|value| value.to_str().ok()),
        Some(SUBPROTOCOL),
    );
}

#[test]
fn the_callback_accepts_a_client_offering_nothing() {
    let response = negotiate(None).expect("no preference is accepted");
    assert!(
        response
            .headers()
            .get(header::SEC_WEBSOCKET_PROTOCOL)
            .is_none(),
        "no subprotocol is added when none was offered",
    );
}

#[test]
fn the_callback_refuses_an_unsupported_subprotocol_with_403() {
    let response = negotiate(Some("agent.eventbus.v2")).expect_err("an unknown version is refused");
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn binding_creates_a_private_directory_and_socket() {
    let dir = SocketDir::new("modes");
    let listener = Listener::bind(dir.socket(), WebSocketConfig::default()).expect("binds");

    assert_eq!(mode(&dir.0), 0o700, "the directory is 0700");
    assert_eq!(mode(listener.path()), 0o600, "the socket is 0600");
    assert_eq!(listener.path(), dir.socket());
}

#[tokio::test]
async fn a_second_bind_reports_the_socket_in_use() {
    let dir = SocketDir::new("in-use");
    let _first = Listener::bind(dir.socket(), WebSocketConfig::default()).expect("binds");

    let error = Listener::bind(dir.socket(), WebSocketConfig::default())
        .expect_err("a live socket is not stolen");
    assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
}

#[tokio::test]
async fn a_stale_socket_is_recreated() {
    let dir = SocketDir::new("stale");
    let path = dir.socket();
    std::fs::create_dir_all(&dir.0).expect("creates the directory");

    // A plain file at the socket path is stale: nothing accepts on it.
    std::fs::write(&path, b"stale").expect("writes a stale file");
    let listener = Listener::bind(&path, WebSocketConfig::default()).expect("reclaims the path");
    assert_eq!(mode(listener.path()), 0o600);
}

#[tokio::test]
async fn a_client_that_offers_the_subprotocol_is_echoed_it() {
    let dir = SocketDir::new("subprotocol");
    let listener = Listener::bind(dir.socket(), WebSocketConfig::default()).expect("binds");
    let path = dir.socket();

    let client = tokio::spawn(async move {
        let stream = UnixStream::connect(&path).await.expect("connects");
        let mut request = "ws://localhost/".into_client_request().expect("request");
        request.headers_mut().insert(
            header::SEC_WEBSOCKET_PROTOCOL,
            HeaderValue::from_static(SUBPROTOCOL),
        );
        tokio_tungstenite::client_async(request, stream)
            .await
            .expect("handshakes")
    });

    let connection = accept_bounded(&listener).await;
    assert_eq!(connection.credential().uid, owner_uid(&dir.0));
    let (_, response) = client.await.expect("client task");
    assert_eq!(
        response
            .headers()
            .get(header::SEC_WEBSOCKET_PROTOCOL)
            .and_then(|value| value.to_str().ok()),
        Some(SUBPROTOCOL),
    );
}

#[tokio::test]
async fn a_client_that_offers_no_subprotocol_is_accepted_without_one() {
    let dir = SocketDir::new("no-subprotocol");
    let listener = Listener::bind(dir.socket(), WebSocketConfig::default()).expect("binds");
    let path = dir.socket();

    let client = tokio::spawn(async move {
        let stream = UnixStream::connect(&path).await.expect("connects");
        tokio_tungstenite::client_async("ws://localhost/", stream)
            .await
            .expect("handshakes")
    });

    let _connection = accept_bounded(&listener).await;
    let (_, response) = client.await.expect("client task");
    assert!(
        response
            .headers()
            .get(header::SEC_WEBSOCKET_PROTOCOL)
            .is_none(),
        "no subprotocol is echoed when none was offered",
    );
}

#[tokio::test]
async fn an_unsupported_subprotocol_is_refused_and_the_loop_continues() {
    let dir = SocketDir::new("bad-subprotocol");
    let listener = Listener::bind(dir.socket(), WebSocketConfig::default()).expect("binds");
    let path = dir.socket();

    // A client offering a version we do not speak is refused during the
    // handshake, before any application frame is read.
    let refused_path = path.clone();
    let refused = tokio::spawn(async move {
        let stream = UnixStream::connect(&refused_path).await.expect("connects");
        let mut request = "ws://localhost/".into_client_request().expect("request");
        request.headers_mut().insert(
            header::SEC_WEBSOCKET_PROTOCOL,
            HeaderValue::from_static("agent.eventbus.v2"),
        );
        tokio_tungstenite::client_async(request, stream).await
    });

    // The accept loop skips the refused peer; it only resolves once a valid
    // client arrives, so run it in the background.
    let accept = tokio::spawn(async move { listener.accept().await.map(|c| c.credential()) });

    // The refusal happens first, which also proves `accept` pulled and dropped
    // the bad peer before serving anyone.
    let refused_result = refused.await.expect("refused task");
    assert!(refused_result.is_err(), "the bad subprotocol is refused");

    let good_path = path;
    let good = tokio::spawn(async move {
        let stream = UnixStream::connect(&good_path).await.expect("connects");
        let mut request = "ws://localhost/".into_client_request().expect("request");
        request.headers_mut().insert(
            header::SEC_WEBSOCKET_PROTOCOL,
            HeaderValue::from_static(SUBPROTOCOL),
        );
        tokio_tungstenite::client_async(request, stream).await
    });

    let credential = accept
        .await
        .expect("accept task")
        .expect("accepts the good client");
    // The peer ran as this process, so its credential is this process's.
    assert_eq!(credential.uid, owner_uid(&dir.0));
    assert!(good.await.expect("good task").is_ok());
}

/// The UID that owns `path`.
///
/// The socket we bound is owned by this process, so its owner is the UID the
/// peer credential must report.
fn owner_uid(path: &Path) -> u32 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path)
        .expect("the socket directory has metadata")
        .uid()
}
