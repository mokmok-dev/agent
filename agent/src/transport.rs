//! Transport: UDS listener, WebSocket handshake, and peer credentials.
//!
//! Implements `docs/event-bus/transport.md`. The bus listens on a pathname Unix
//! domain socket (not an abstract socket) so filesystem permissions apply, reads
//! the peer credentials, and completes an RFC 6455 WebSocket handshake. The
//! subprotocol is negotiated by the `agent.eventbus.v1` string.
//!
//! The protocol layer that turns WebSocket messages into `publish`/`subscribe`
//! traffic is the next milestone; this module ends at an accepted, authorized
//! connection and its credential.
//!
//! Security, per `docs/event-bus/security.md`:
//!
//! - The parent directory is created `0700` and the socket file `0600`, so only
//!   the bus user can connect.
//! - The peer's credentials (UID, GID, and PID where the platform reports it)
//!   are captured for the [`Allowlist`] the caller applies.
//!
//! Unix-only: the transport is a Unix domain socket.

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::handshake::server::{
    Callback, ErrorResponse, Request, Response,
};
use tokio_tungstenite::tungstenite::http::{HeaderValue, StatusCode, header};

mod allowlist;

pub use allowlist::{Allowlist, PeerCredential};
pub use tokio_tungstenite::tungstenite::Message;
pub use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

/// The WebSocket subprotocol this build speaks.
pub const SUBPROTOCOL: &str = "agent.eventbus.v1";

/// Default cap on concurrently open connections.
pub const DEFAULT_MAX_CONNECTIONS: usize = 1024;

/// A bound, not-yet-awaiting UDS listener.
#[derive(Debug)]
pub struct Listener {
    inner: UnixListener,
    path: PathBuf,
    config: WebSocketConfig,
    permits: Arc<Semaphore>,
}

impl Listener {
    /// Bind a listener at `path`, creating the parent directory `0700` and the
    /// socket `0600`, with the default connection cap.
    ///
    /// # Errors
    ///
    /// As [`Listener::bind_with`].
    pub fn bind(
        path: impl AsRef<Path>,
        config: WebSocketConfig,
    ) -> io::Result<Self> {
        Self::bind_with(path, config, DEFAULT_MAX_CONNECTIONS)
    }

    /// Bind a listener with an explicit connection cap.
    ///
    /// The listener is removed and recreated on startup. A stale socket left by
    /// a crash is removed; if another process is already accepting on `path`,
    /// binding fails with [`io::ErrorKind::AddrInUse`] rather than stealing the
    /// socket.
    ///
    /// # Errors
    ///
    /// Returns an [`io::Error`] if the parent directory cannot be created, a
    /// running bus owns `path`, or the socket cannot be bound.
    pub fn bind_with(
        path: impl AsRef<Path>,
        config: WebSocketConfig,
        max_connections: usize,
    ) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
        }

        if let Some(existing) = stale_socket(&path)? {
            std::fs::remove_file(existing)?;
        }

        let inner = UnixListener::bind(&path)?;
        // The socket is chmod'd after binding because the bind mode is masked by
        // the process umask.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;

        Ok(Self {
            inner,
            path,
            config,
            permits: Arc::new(Semaphore::new(max_connections.max(1))),
        })
    }

    /// The socket path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Accept the next connection and complete the WebSocket handshake.
    ///
    /// The caller applies its [`Allowlist`] to the returned
    /// [`Connection::credential`] before processing any application message. A
    /// peer that fails the handshake or is refused the subprotocol consumes no
    /// connection permit and is skipped.
    ///
    /// # Errors
    ///
    /// Returns an [`io::Error`] if accepting from the socket fails or the
    /// connection cap is reached. A peer that fails the handshake is skipped,
    /// not returned as an error: the listener is healthy, and one port probe or
    /// truncated request must not stop the bus.
    pub async fn accept(&self) -> io::Result<Connection> {
        loop {
            let permit = Arc::clone(&self.permits).try_acquire_owned().map_err(|_| {
                io::Error::new(io::ErrorKind::WouldBlock, "connection limit reached")
            })?;
            let (stream, _addr) = self.inner.accept().await?;
            let credential = peer_credential(&stream)?;

            match tokio_tungstenite::accept_hdr_async_with_config(
                stream,
                ProtocolCallback,
                Some(self.config),
            )
            .await
            {
                Ok(websocket) => {
                    return Ok(Connection {
                        credential,
                        websocket,
                        _permit: permit,
                    });
                },
                // A failed handshake — an unsupported subprotocol, a plain
                // connect probe, a truncated request — is the peer's problem,
                // not the listener's. Drop the permit and wait for the next.
                Err(error) => {
                    tracing::debug!(
                        uid = credential.uid,
                        %error,
                        "skipped a peer that failed the handshake",
                    );
                },
            }
        }
    }
}

/// The peer credentials of an accepted stream.
fn peer_credential(stream: &UnixStream) -> io::Result<PeerCredential> {
    let cred = stream.peer_cred()?;
    Ok(PeerCredential {
        uid: cred.uid(),
        gid: cred.gid(),
        pid: cred.pid().and_then(|pid| u32::try_from(pid).ok()),
    })
}

/// Decide what to do about a socket file that already exists at `path`.
///
/// Returns `Ok(Some(path))` when the socket is stale and must be removed, `Ok(None)`
/// when the path does not exist, and `Err(AddrInUse)` when another process is
/// accepting on it. The probe is a blocking `connect`, which is fine at startup.
fn stale_socket(path: &Path) -> io::Result<Option<PathBuf>> {
    use std::os::unix::net::UnixStream as StdUnixStream;

    if !path.exists() {
        return Ok(None);
    }
    if StdUnixStream::connect(path).is_ok() {
        // Something accepted the connection, so a bus is live.
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!("another bus is listening on {}", path.display()),
        ));
    }
    // Nothing is listening; the socket is stale.
    Ok(Some(path.to_path_buf()))
}

/// An accepted WebSocket connection and the peer it came from.
#[derive(Debug)]
pub struct Connection {
    credential: PeerCredential,
    websocket: WebSocketStream<UnixStream>,
    /// Held for the connection's lifetime so the accept loop bounds the number
    /// of open connections.
    _permit: OwnedSemaphorePermit,
}

impl Connection {
    /// The peer's credentials.
    #[must_use]
    pub const fn credential(&self) -> PeerCredential {
        self.credential
    }

    /// The underlying WebSocket stream.
    #[must_use]
    pub const fn websocket(&mut self) -> &mut WebSocketStream<UnixStream> {
        &mut self.websocket
    }

    /// Split the connection into its credential and the WebSocket stream.
    #[must_use]
    pub fn into_parts(self) -> (PeerCredential, WebSocketStream<UnixStream>) {
        (self.credential, self.websocket)
    }
}

/// The handshake callback that enforces the subprotocol.
///
/// The peer may offer `agent.eventbus.v1` (or no preference); the reply names it
/// only when the client offered it. Any other offer is refused with
/// `403 Forbidden`, which [`Listener::accept`] recognizes and skips.
///
/// Per `docs/event-bus/transport.md`, the transport is a UDS with no browser
/// `Origin` to validate, so the callback does not inspect `Origin`.
#[derive(Debug, Clone, Copy)]
struct ProtocolCallback;

impl Callback for ProtocolCallback {
    fn on_request(
        self,
        request: &Request,
        mut response: Response,
    ) -> Result<Response, ErrorResponse> {
        let mut offered_supported = false;
        for protocol in request
            .headers()
            .get_all(header::SEC_WEBSOCKET_PROTOCOL)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|value| value.split(','))
            .map(str::trim)
        {
            if protocol == SUBPROTOCOL {
                offered_supported = true;
            } else {
                return Err(forbidden("unsupported subprotocol"));
            }
        }

        // Echo the subprotocol only when the client offered it; a client that
        // offered none is accepted without one.
        if offered_supported {
            response.headers_mut().insert(
                header::SEC_WEBSOCKET_PROTOCOL,
                HeaderValue::from_static(SUBPROTOCOL),
            );
        }
        Ok(response)
    }
}

/// A `403 Forbidden` handshake rejection with a short reason.
fn forbidden(reason: &str) -> ErrorResponse {
    let mut response = ErrorResponse::new(Some(reason.to_owned()));
    *response.status_mut() = StatusCode::FORBIDDEN;
    response
}

#[cfg(test)]
mod tests {
    // Tests for the transport: socket permissions, stale-socket detection, the
    // subprotocol handshake, and peer credential capture.
    //
    // These drive the async listener on a real runtime, so the module is `async`.

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
        let response =
            negotiate(Some("agent.eventbus.v2")).expect_err("an unknown version is refused");
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
        let listener =
            Listener::bind(&path, WebSocketConfig::default()).expect("reclaims the path");
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

    #[tokio::test]
    async fn a_bare_connect_probe_does_not_stop_the_listener() {
        let dir = SocketDir::new("probe");
        let listener = Listener::bind(dir.socket(), WebSocketConfig::default()).expect("binds");
        let path = dir.socket();

        // A health check that connects and disconnects without a handshake must
        // not take the listener down. This is the probe the daemon's own
        // readiness check uses.
        let probe = tokio::spawn(async move {
            let stream = UnixStream::connect(&path).await.expect("connects");
            drop(stream);
        });

        // A real client arriving afterwards is still served.
        let good_path = dir.socket();
        let good = tokio::spawn(async move {
            let stream = UnixStream::connect(&good_path).await.expect("connects");
            let mut request = "ws://localhost/".into_client_request().expect("request");
            request.headers_mut().insert(
                header::SEC_WEBSOCKET_PROTOCOL,
                HeaderValue::from_static(SUBPROTOCOL),
            );
            tokio_tungstenite::client_async(request, stream).await
        });

        probe.await.expect("probe task");
        let connection = listener.accept().await.expect("accepts the real client");
        assert_eq!(connection.credential().uid, owner_uid(&dir.0));
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
}
