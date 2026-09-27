//! The egress `CONNECT` proxy: the one route out of a confined command.
//!
//! The proxy listens on a Unix domain socket (or loopback TCP on a host without
//! a network namespace) and speaks just enough HTTP to establish a tunnel:
//!
//! 1. read the request head under a size cap and a deadline,
//! 2. authenticate the per-proxy token,
//! 3. consult the destination set, and
//! 4. on a permitted destination, open the upstream connection, answer
//!    `200 Connection Established`, and copy bytes both ways.
//!
//! The tunnel is **opaque**: the proxy does not terminate TLS and never sees the
//! provider credentials, which the command holds and sends end to end. See
//! `docs/sandbox/network.md`.
//!
//! Approval for an unlisted destination is milestone 5. Today an unlisted
//! destination is refused with `403`, which the design calls the behaviour with
//! no approver configured.

mod destinations;
mod request;

use std::io::{BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub use destinations::DestinationSet;
pub use request::{Connect, ParseError};

/// How long the proxy waits for a request head before giving up.
pub const HEAD_TIMEOUT: Duration = Duration::from_secs(10);

/// Why a proxy could not be started or stopped.
#[derive(Debug, thiserror::Error)]
pub enum ProxyError {
    /// The socket could not be bound or its directory prepared.
    #[error("could not bind the proxy socket: {0}")]
    Bind(#[source] std::io::Error),
    /// The proxy is not configured with a transport.
    #[error("the proxy has no socket path and no loopback port")]
    NoTransport,
}

/// Where the proxy listens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transport {
    /// A Unix domain socket, which crosses a network namespace. Linux.
    UnixSocket(PathBuf),
    /// Loopback TCP on `port`. The weaker form for a host without a namespace.
    Loopback(u16),
}

/// The configuration of one proxy: its transport, its token, and its allowlist.
#[derive(Debug, Clone)]
pub struct ProxyConfig {
    /// Where the proxy listens.
    pub transport: Transport,
    /// The token a client must present. Generated per proxy and dies with it.
    pub token: String,
    /// The destinations the proxy permits.
    pub destinations: DestinationSet,
}

/// What the proxy decided about a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// The destination is permitted; the tunnel is open.
    Tunnelled,
    /// The token was missing or wrong.
    Unauthorized,
    /// The destination is not permitted.
    Forbidden,
    /// The request head was malformed or oversized.
    Malformed(String),
    /// The upstream connection could not be opened.
    UpstreamFailed(String),
}

/// A running proxy on a Unix domain socket, removed on drop.
#[derive(Debug)]
pub struct UnixProxy {
    listener: UnixListener,
    path: PathBuf,
    config: ProxyConfig,
}

impl UnixProxy {
    /// Bind a proxy at `path`, creating the parent directory `0700` and the
    /// socket `0600`.
    ///
    /// # Errors
    ///
    /// Returns [`ProxyError::Bind`] if the directory or socket cannot be created.
    pub fn bind(
        path: impl AsRef<Path>,
        config: ProxyConfig,
    ) -> Result<Self, ProxyError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(ProxyError::Bind)?;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
                .map_err(ProxyError::Bind)?;
        }
        // A stale socket from a crash is removed; a live one is not stolen.
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).map_err(ProxyError::Bind)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .map_err(ProxyError::Bind)?;
        Ok(Self {
            listener,
            path,
            config,
        })
    }

    /// The socket path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The proxy's configuration.
    #[must_use]
    pub const fn config(&self) -> &ProxyConfig {
        &self.config
    }

    /// Accept and serve one connection, returning its [`Decision`].
    ///
    /// # Errors
    ///
    /// Returns an [`std::io::Error`] if accepting or the handshake fails at the
    /// socket level; a decision-level refusal is returned as [`Decision`], not an
    /// error.
    pub fn serve_one(&self) -> std::io::Result<Decision> {
        let (stream, _addr) = self.listener.accept()?;
        serve(&stream, &self.config)
    }

    /// The underlying listener, for a caller that drives the accept loop.
    #[must_use]
    pub const fn listener(&self) -> &UnixListener {
        &self.listener
    }
}

impl Drop for UnixProxy {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Serve one already-accepted Unix connection.
///
/// # Errors
///
/// Returns an [`std::io::Error`] if a socket operation fails.
pub fn serve(
    stream: &UnixStream,
    config: &ProxyConfig,
) -> std::io::Result<Decision> {
    // A pre-authentication client must not be able to stall the proxy: the head
    // is read under a deadline, and the client end is shut down when it passes.
    stream.set_read_timeout(Some(HEAD_TIMEOUT))?;
    stream.set_write_timeout(Some(HEAD_TIMEOUT))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let head = match read_head(&mut reader) {
        Ok(head) => head,
        Err(error) => {
            let decision = Decision::Malformed(error.to_string());
            write_response(reader.get_mut(), &decision)?;
            return Ok(decision);
        },
    };
    let (connect, _consumed) = match request::parse(&head) {
        Ok(parsed) => parsed,
        Err(error) => {
            let decision = Decision::Malformed(error.to_string());
            write_response(reader.get_mut(), &decision)?;
            return Ok(decision);
        },
    };

    let decision = authorize_and_decide(&connect, config);
    if decision != Decision::Tunnelled {
        write_response(reader.get_mut(), &decision)?;
        return Ok(decision);
    }

    // Open the upstream before answering, so a failure is a clean `502` rather
    // than a `200` followed by a dead tunnel.
    let mut upstream = match open_upstream(&connect.host, connect.port) {
        Ok(upstream) => upstream,
        Err(error) => {
            let decision = Decision::UpstreamFailed(error.to_string());
            write_response(reader.get_mut(), &decision)?;
            return Ok(decision);
        },
    };

    write_response(reader.get_mut(), &Decision::Tunnelled)?;

    // The head deadline was for the handshake only; a long-lived tunnel must not
    // be torn down by it.
    reader.get_mut().set_read_timeout(None)?;
    reader.get_mut().set_write_timeout(None)?;

    // The buffered reader may have read bytes past the head — the start of the
    // tunnel. Those must reach the upstream before the copy threads start, or
    // they are lost. Writing an empty slice is a no-op, so this is unconditional.
    upstream.write_all(reader.buffer())?;
    let client = reader.into_inner();
    tunnel(&client, &upstream)?;
    Ok(Decision::Tunnelled)
}

/// The authorization and destination decision for a parsed request.
fn authorize_and_decide(
    connect: &Connect,
    config: &ProxyConfig,
) -> Decision {
    // The token is checked before the destination, so a client that cannot
    // authenticate learns nothing about the allowlist.
    let expected = format!("Bearer {}", config.token);
    let authorized = connect
        .authorization
        .as_deref()
        .is_some_and(|value| value == expected);
    if !authorized {
        return Decision::Unauthorized;
    }
    if config.destinations.permits(&connect.host, connect.port) {
        Decision::Tunnelled
    } else {
        Decision::Forbidden
    }
}

/// Whether `len` bytes of head exceed the cap.
///
/// A head of exactly [`request::MAX_HEAD_BYTES`] is allowed; one byte more is
/// not. Kept as one function so the boundary is tested once, directly, rather
/// than only through a socket.
const fn exceeds_cap(len: usize) -> bool {
    len > request::MAX_HEAD_BYTES
}

/// Read the request head, up to and including the blank line.
///
/// Reads bytes, not UTF-8 lines, so a head containing non-UTF-8 bytes is read to
/// its blank line and rejected by [`request::parse`] as malformed rather than
/// truncating the read.
fn read_head(reader: &mut BufReader<UnixStream>) -> Result<Vec<u8>, ParseError> {
    let mut head = Vec::new();
    loop {
        let mut byte = [0_u8; 1];
        let read = reader.read(&mut byte).map_err(|_| ParseError::Incomplete)?;
        if read == 0 {
            return Err(ParseError::Incomplete);
        }
        head.push(byte[0]);
        if exceeds_cap(head.len()) {
            return Err(ParseError::HeadTooLarge);
        }
        if head_is_complete(&head) {
            return Ok(head);
        }
    }
}

/// Whether `head` ends at the blank line that terminates it.
///
/// The blank line is `LF LF` or `LF CR LF`, so the check is a disjunction; a
/// head ending in either is complete. Kept separate from the socket loop so the
/// three-way shape (LF, CRLF, neither) is tested directly rather than only by
/// driving a connection to its read timeout.
fn head_is_complete(head: &[u8]) -> bool {
    head.ends_with(b"\n\n") || head.ends_with(b"\n\r\n")
}

/// Open a TCP connection to `host:port` on the trusted side.
fn open_upstream(
    host: &str,
    port: u16,
) -> std::io::Result<TcpStream> {
    let mut last_error = None;
    for address in (host, port).to_socket_addrs()? {
        match TcpStream::connect_timeout(&address, HEAD_TIMEOUT) {
            Ok(stream) => return Ok(stream),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "no address resolved")
    }))
}

/// Copy bytes both ways until both directions close, honouring half-close.
///
/// When one side finishes writing, the other's write half is shut down and the
/// remaining direction keeps flowing, so a client half-close still receives the
/// reply.
fn tunnel(
    client: &UnixStream,
    upstream: &TcpStream,
) -> std::io::Result<()> {
    let mut upstream_read = upstream.try_clone()?;
    let mut upstream_write = upstream.try_clone()?;
    let mut client_read = client.try_clone()?;
    let mut client_write = client.try_clone()?;

    // Client -> upstream, then shut the upstream write half.
    let to_upstream = std::thread::spawn(move || {
        let _ = std::io::copy(&mut client_read, &mut upstream_write);
        let _ = upstream_write.shutdown(std::net::Shutdown::Write);
    });
    // Upstream -> client, then shut the client write half.
    let to_client = std::thread::spawn(move || {
        let _ = std::io::copy(&mut upstream_read, &mut client_write);
        let _ = client_write.shutdown(std::net::Shutdown::Write);
    });
    let _ = to_upstream.join();
    let _ = to_client.join();
    Ok(())
}

/// Write the HTTP response for `decision`.
fn write_response(
    stream: &mut UnixStream,
    decision: &Decision,
) -> std::io::Result<()> {
    let message = match decision {
        Decision::Tunnelled => "HTTP/1.1 200 Connection Established\r\n\r\n",
        Decision::Unauthorized => {
            "HTTP/1.1 407 Proxy Authentication Required\r\n\
             Proxy-Authenticate: Bearer\r\n\r\n"
        },
        Decision::Forbidden => "HTTP/1.1 403 Forbidden\r\n\r\n",
        Decision::Malformed(_) => "HTTP/1.1 400 Bad Request\r\n\r\n",
        Decision::UpstreamFailed(_) => "HTTP/1.1 502 Bad Gateway\r\n\r\n",
    };
    stream.write_all(message.as_bytes())?;
    stream.flush()
}

/// A `Proxy-Authorization` value for `token`, as the proxy expects it.
#[must_use]
pub fn bearer(token: &str) -> String {
    format!("Bearer {token}")
}

/// The `HTTP_PROXY` URL for a loopback proxy on `port` with `token`.
///
/// The URL is kept out of logs; see `docs/sandbox/security.md`.
#[must_use]
pub fn proxy_url(
    port: u16,
    token: &str,
) -> String {
    format!("http://agent:{token}@127.0.0.1:{port}")
}

#[cfg(test)]
mod tests {
    // Unit tests for the pieces that need no socket: the head-size boundary and
    // the URL helpers, plus a direct drop test so `UnixProxy::drop` is covered
    // without the integration test's directory cleanup masking it.

    use super::*;

    #[test]
    fn the_head_cap_boundary_is_inclusive() {
        assert!(!exceeds_cap(request::MAX_HEAD_BYTES));
        assert!(exceeds_cap(request::MAX_HEAD_BYTES + 1));
        assert!(!exceeds_cap(0));
    }

    #[test]
    fn a_head_is_complete_at_either_blank_line() {
        // The disjunction is real: each ending alone is complete.
        assert!(head_is_complete(b"CONNECT h:1 HTTP/1.1\n\n"));
        assert!(head_is_complete(b"CONNECT h:1 HTTP/1.1\r\n\r\n"));
    }

    #[test]
    fn a_head_mid_line_is_not_complete() {
        assert!(!head_is_complete(b"CONNECT h:1 HTTP/1.1\n"));
        assert!(!head_is_complete(b"CONNECT h:1 HTTP/1.1\r\n"));
        assert!(!head_is_complete(b"CONNECT h:1 HTTP/1.1\n\r"));
        assert!(!head_is_complete(b""));
        assert!(!head_is_complete(b"\n"));
    }

    #[test]
    fn the_output_cap_constant_is_eight_kib() {
        assert_eq!(request::MAX_HEAD_BYTES, 8192);
    }

    #[test]
    fn bearer_formats_the_expected_header_value() {
        assert_eq!(bearer("tok"), "Bearer tok");
    }

    #[test]
    fn the_proxy_url_carries_the_token_and_port() {
        assert_eq!(proxy_url(8080, "t"), "http://agent:t@127.0.0.1:8080");
    }

    #[test]
    fn binding_creates_a_socket_and_drop_removes_it() {
        // A bare `UnixProxy`, not the integration test's guard, so nothing else
        // removes the socket and `Drop` is the only thing that can.
        let root = std::env::temp_dir().join(format!("sandbox-proxy-drop-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let socket = root.join("e.sock");
        let config = ProxyConfig {
            transport: Transport::UnixSocket(socket.clone()),
            token: "t".to_owned(),
            destinations: DestinationSet::empty(),
        };

        let path;
        {
            let proxy = UnixProxy::bind(&socket, config).expect("binds");
            path = proxy.path().to_path_buf();
            assert!(path.exists(), "the socket exists while the proxy is alive");
        }
        assert!(!path.exists(), "the socket is removed when the proxy drops");
        let _ = std::fs::remove_dir_all(&root);
    }
}
