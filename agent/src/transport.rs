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
use tokio_tungstenite::tungstenite::Error as WsError;
use tokio_tungstenite::tungstenite::handshake::server::{
    Callback, ErrorResponse, Request, Response,
};
use tokio_tungstenite::tungstenite::http::{HeaderValue, StatusCode, header};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

mod allowlist;

pub use allowlist::{Allowlist, PeerCredential};
pub use tokio_tungstenite::tungstenite::Message;

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
    /// connection cap is reached. A non-recoverable handshake failure is
    /// returned as an `io::Error`.
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
                // The callback refused the subprotocol; skip this peer and keep
                // serving. The permit is dropped when `permit` goes out of
                // scope at the end of this iteration.
                Err(WsError::Http(response)) if response.status() == StatusCode::FORBIDDEN => {},
                Err(error) => return Err(io::Error::other(error)),
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
mod tests;
