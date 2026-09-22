//! The daemon's CONNECT proxy: the single egress path a confined command may
//! use. The design and trust model are in `docs/egress.md`.
//!
//! It requires a per-proxy credential so only the sandbox it was started for
//! can use it, not any other process of the same user. It does not terminate
//! TLS, so the provider credentials stay end to end.
//!
//! Two transports: a loopback **TCP** port (macOS Seatbelt, where the child
//! reaches `127.0.0.1`), and a **Unix socket** (Linux, where the child runs in a
//! private network namespace with no IP route and reaches the mounted socket).

use std::io;
use std::net::SocketAddr;
use std::os::unix::fs::FileTypeExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use agentd_events::{Event, EventLog, LogEntry, Traceparent};
use base64::Engine as _;
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt as _, AsyncRead, AsyncWrite, AsyncWriteExt as _, BufReader,
};
use tokio::net::{TcpListener, TcpStream, UnixListener};
use tokio::sync::Semaphore;

use agentd_sandbox::{EnvVar, HostPort, Policy, ProxyGrant};

/// A client asks to reach a `host:port` that the static allowlist does not name.
/// Reserved to daemon-authority publishers, so an agent cannot approve itself.
pub const EGRESS_REQUESTED: &str = "session.egress.requested";
/// An approver's grant for an [`EGRESS_REQUESTED`], correlated by `request_id`.
pub const EGRESS_GRANTED: &str = "session.egress.granted";
/// An approver's denial for an [`EGRESS_REQUESTED`], correlated by `request_id`.
pub const EGRESS_DENIED: &str = "session.egress.denied";
/// Nobody decided an [`EGRESS_REQUESTED`] before the deadline, so it was
/// withdrawn. The tunnel is not opened.
pub const EGRESS_CANCELLED: &str = "session.egress.cancelled";

/// The largest request head (request line plus headers) the proxy reads before
/// rejecting a connection. Bounds a pre-authentication client's memory.
const MAX_HEADER_BYTES: usize = 16 * 1024;
/// How long a client has to send its request head before the proxy gives up.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// The most concurrent client connections the proxy serves.
const MAX_CONNECTIONS: usize = 256;
/// The loopback port the child's forwarder listens on, fixed so the injected
/// proxy URL is stable. A Unix-socket proxy's policy port must be this.
pub const FORWARD_PORT: u16 = 31_828;

/// The proxy allowlist: the `host:port` destinations a tunnel may open.
///
/// A destination on the static allowlist is granted immediately. Anything else
/// is, when an approver is configured, put to the human: a
/// [`EGRESS_REQUESTED`] event goes on the log and the proxy waits for a matching
/// [`EGRESS_GRANTED`]/[`EGRESS_DENIED`] under the same `request_id`. With no
/// approver the unknown destination is denied. This is the safe way to let an
/// agent ask for a host the operator did not pre-author, because the tunnel is
/// the *only* egress path (the sandbox runs in a private network namespace), so
/// the request cannot be bypassed.
#[derive(Clone)]
pub struct Egress {
    allowed: Arc<Vec<HostPort>>,
    /// The log to ask on, and the timeout to wait for a decision; `None` denies
    /// unknown hosts outright.
    approver: Option<(EventLog, Duration)>,
}

impl std::fmt::Debug for Egress {
    fn fmt(
        &self,
        formatter: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        formatter
            .debug_struct("Egress")
            .field("allowed", &self.allowed)
            .field("has_approver", &self.approver.is_some())
            .finish()
    }
}

impl Default for Egress {
    fn default() -> Self {
        Self {
            allowed: Arc::new(Vec::new()),
            approver: None,
        }
    }
}

impl Egress {
    /// Builds a static allowlist; a destination not on it is denied.
    #[must_use]
    pub fn new(destinations: Vec<HostPort>) -> Self {
        Self {
            allowed: Arc::new(destinations),
            approver: None,
        }
    }

    /// Adds an approver: a destination not on the static allowlist is put to the
    /// log and waited on for `timeout`, then denied if no decision arrives.
    #[must_use]
    pub fn with_approver(
        mut self,
        log: EventLog,
        timeout: Duration,
    ) -> Self {
        self.approver = Some((log, timeout));
        self
    }

    /// Whether a tunnel to `host:port` is on the static allowlist.
    #[must_use]
    fn listed(
        &self,
        host: &str,
        port: u16,
    ) -> bool {
        self.allowed.iter().any(|destination| {
            destination.port == port && destination.host.eq_ignore_ascii_case(host)
        })
    }

    /// The destinations the **static** allowlist names.
    ///
    /// This is the pre-approved set, not the set the proxy will permit: with an
    /// approver configured, a destination outside it can still be granted at run
    /// time. A policy that carries it therefore records the static rules, not a
    /// closed allowlist.
    #[must_use]
    pub fn static_allowlist(&self) -> &[HostPort] {
        &self.allowed
    }

    /// Whether a tunnel to `host:port` is permitted, asking an approver when it
    /// is not on the static allowlist.
    async fn allows(
        &self,
        host: &str,
        port: u16,
        traceparent: Option<&str>,
    ) -> bool {
        if self.listed(host, port) {
            return true;
        }
        let Some((log, timeout)) = &self.approver else {
            return false;
        };
        self.ask(log, host, port, *timeout, traceparent).await
    }

    /// Publishes a request and waits for a decision, cancelling on timeout.
    ///
    /// The child's `traceparent`, when it sent one, is recorded on the approval
    /// event so the request, the approval, and the child's own span share one
    /// trace.
    async fn ask(
        &self,
        log: &EventLog,
        host: &str,
        port: u16,
        timeout: Duration,
        traceparent: Option<&str>,
    ) -> bool {
        // Subscribe before publishing, so a fast decision is not missed.
        let mut decisions = log.subscribe();
        let request_id = uuid::Uuid::now_v7().to_string();
        let mut event = Event::new(
            EGRESS_REQUESTED,
            serde_json::json!({ "request_id": request_id, "host": host, "port": port }),
        );
        if let Some(traceparent) = traceparent.and_then(|value| Traceparent::parse(value).ok()) {
            event = event.with_traceparent(&traceparent);
        }
        if let Err(error) = log.publish(event).await {
            tracing::error!(%error, "failed to record an egress request");
            return false;
        }
        if await_egress_decision(&mut decisions, &request_id, timeout).await {
            return true;
        }
        // Record the cancellation: nobody decided within the deadline. An
        // approver's own decision is already in the log, and a refusal is its
        // own type, so the log never claims an operator refused a request they
        // never saw.
        let mut cancelled = Event::new(
            EGRESS_CANCELLED,
            serde_json::json!({
                "request_id": request_id,
                "decision": "cancelled",
                "host": host,
                "port": port,
                "reason": "no approval within the timeout",
            }),
        );
        if let Some(traceparent) = traceparent.and_then(|value| Traceparent::parse(value).ok()) {
            cancelled = cancelled.with_traceparent(&traceparent);
        }
        if let Err(error) = log.publish(cancelled).await {
            tracing::error!(%error, "failed to record an egress cancellation");
        }
        false
    }
}

/// Waits for a decision on `request_id`: the same shape as the sandbox approval
/// (`check` for the exact id, ignore unrelated events, a lagged subscriber keeps
/// waiting, the bus closing or the timeout is a refusal).
async fn await_egress_decision(
    receiver: &mut tokio::sync::broadcast::Receiver<LogEntry>,
    request_id: &str,
    timeout: Duration,
) -> bool {
    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            () = &mut deadline => return false,
            incoming = receiver.recv() => match incoming {
                Ok(recorded) => {
                    let event = recorded.event;
                    let matches = event
                        .data
                        .get("request_id")
                        .and_then(serde_json::Value::as_str)
                        == Some(request_id);
                    if !matches {
                        continue;
                    }
                    match event.r#type.as_str() {
                        EGRESS_GRANTED => return true,
                        EGRESS_DENIED | EGRESS_CANCELLED => return false,
                        _ => {},
                    }
                },
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {},
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return false,
            },
        }
    }
}

/// Where the proxy listens.
enum Transport {
    /// A loopback TCP port (macOS Seatbelt: no private namespace).
    Tcp(SocketAddr),
    /// A Unix socket path (Linux: crosses the private network namespace).
    Unix(PathBuf),
}

/// A running proxy: where it listens and its shutdown handle.
pub struct Proxy {
    transport: Transport,
    /// The token the client must present; embedded in [`url`](Proxy::url).
    token: String,
    shutdown: tokio::sync::watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}

impl Proxy {
    /// Binds `127.0.0.1:0` and serves `egress` on it with a fresh credential.
    ///
    /// # Errors
    ///
    /// Returns the bind error when no loopback port can be taken.
    pub async fn start(egress: Egress) -> io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let address = listener.local_addr()?;
        let token = uuid::Uuid::now_v7().to_string();
        let expected = basic_authorization(&token);
        let permits = Arc::new(Semaphore::new(MAX_CONNECTIONS));
        let (shutdown, mut shutdown_rx) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = shutdown_rx.changed() => return,
                    accepted = listener.accept() => {
                        let Ok((stream, _peer)) = accepted else {
                            return;
                        };
                        let Some(permit) = take_permit(&permits, &stream) else {
                            continue;
                        };
                        let egress = egress.clone();
                        let expected = expected.clone();
                        tokio::spawn(async move {
                            let _permit = permit;
                            if let Err(error) = serve(stream, &egress, &expected).await {
                                tracing::debug!(%error, "a proxy connection ended");
                            }
                        });
                    },
                }
            }
        });
        Ok(Self {
            transport: Transport::Tcp(address),
            token,
            shutdown,
            task,
        })
    }

    /// Binds `path` and serves `egress` on that Unix socket with a fresh
    /// credential.
    ///
    /// The child gets the socket bind-mounted and reaches the proxy from inside
    /// a private network namespace, where it has no IP route at all (see
    /// `docs/egress.md`).
    ///
    /// # Errors
    ///
    /// Returns the bind error when the socket path cannot be created.
    pub fn start_unix(
        path: impl AsRef<Path>,
        egress: Egress,
    ) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let listener = UnixListener::bind(&path)?;
        let token = uuid::Uuid::now_v7().to_string();
        let expected = basic_authorization(&token);
        let permits = Arc::new(Semaphore::new(MAX_CONNECTIONS));
        let (shutdown, mut shutdown_rx) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = shutdown_rx.changed() => return,
                    accepted = listener.accept() => {
                        let Ok((stream, _peer)) = accepted else {
                            return;
                        };
                        let Some(permit) = take_permit(&permits, &stream) else {
                            continue;
                        };
                        let egress = egress.clone();
                        let expected = expected.clone();
                        tokio::spawn(async move {
                            let _permit = permit;
                            if let Err(error) = serve(stream, &egress, &expected).await {
                                tracing::debug!(%error, "a proxy connection ended");
                            }
                        });
                    },
                }
            }
        });
        Ok(Self {
            transport: Transport::Unix(path),
            token,
            shutdown,
            task,
        })
    }

    /// Starts the proxy the sandbox's network model asks for and wires `policy`
    /// to it.
    ///
    /// This is the one place the two transports are chosen, because the choice is
    /// a property of the host rather than of the caller:
    ///
    /// - A host that can give the child a **private network namespace** reaches
    ///   the proxy over a **Unix socket** (bind-mounted in, presented on loopback
    ///   by the child's forwarder). On Linux this is the real boundary: the child
    ///   has no IP route at all.
    /// - A host that has no namespace at all (macOS Seatbelt) reaches a
    ///   **loopback TCP** proxy directly, the weaker form the profile grants by
    ///   port.
    /// - **Linux without bubblewrap fails closed.** The loopback-TCP form is for
    ///   a platform that cannot do better; Linux can, so egress is refused rather
    ///   than downgraded to a host-agnostic port grant.
    ///
    /// Either way the proxy's URL is injected as `HTTP_PROXY`/`HTTPS_PROXY`/
    /// `ALL_PROXY`, with `NO_PROXY` so the child's own loopback traffic stays off
    /// the tunnel (without it an ACP agent routes its internal server calls
    /// through the proxy and its session setup fails), and the matching
    /// `network.proxy` grant is set on `policy` so the OS permits the transport.
    /// Any entry the policy already had under those four names is overridden.
    ///
    /// # Errors
    ///
    /// Returns the bind error when the socket path or loopback port cannot be
    /// taken, and an error when Linux lacks the bubblewrap a private namespace
    /// needs.
    ///
    /// The returned handle must be kept for as long as the policy is used:
    /// dropping it stops the listener, and the injected `HTTP_PROXY` would then
    /// name a port nothing serves.
    #[must_use = "the policy now points at this proxy; dropping the handle stops it"]
    pub async fn start_for_policy(
        policy: &mut Policy,
        egress: Egress,
    ) -> io::Result<Self> {
        Self::start_for_policy_with(
            policy,
            egress,
            agentd_sandbox::bubblewrap_available(&policy.fs),
        )
        .await
    }

    /// As [`start_for_policy`](Self::start_for_policy) with the host's
    /// namespace capability supplied, so both transports are testable on one
    /// host.
    async fn start_for_policy_with(
        policy: &mut Policy,
        egress: Egress,
        namespace: bool,
    ) -> io::Result<Self> {
        // On Linux the private namespace is what makes the Unix socket the only
        // route out. Without bubblewrap there is no namespace, and the alternate
        // transport is weaker in a way Linux can avoid: Landlock's port rule has
        // no address dimension, so the granted port is reachable on any address.
        // Fail closed rather than silently downgrade the boundary on a host that
        // can express it correctly.
        if !namespace && cfg!(target_os = "linux") {
            return Err(io::Error::other(
                "egress needs a private network namespace, which requires bubblewrap; \
                 bubblewrap is unavailable",
            ));
        }
        // Captured before `egress` moves into the accept task.
        let destinations = egress.static_allowlist().to_vec();
        let (proxy, socket, port) = if namespace {
            // A private per-proxy directory rather than the shared temp dir: the
            // socket is then reachable only by this user, and the random
            // directory name is not the only thing hiding it. The token stays
            // the real control.
            let dir = std::env::temp_dir().join(format!("agentd-egress-{}", uuid::Uuid::now_v7()));
            std::fs::create_dir(&dir)?;
            let socket = dir.join("proxy.sock");
            let proxy = Self::start_unix(&socket, egress)?;
            // The child's HTTP_PROXY names the forwarder's loopback port, not the
            // socket, so the policy port is `FORWARD_PORT`.
            (proxy, Some(socket), FORWARD_PORT)
        } else {
            let proxy = Self::start(egress).await?;
            // `address()` is `Some` for every TCP transport, so this is an
            // internal-invariant check, not a runtime condition the caller can
            // act on; the workspace forbids panicking on it.
            let Some(address) = proxy.address() else {
                return Err(io::Error::other(
                    "internal error: a loopback TCP proxy bound no address",
                ));
            };
            (proxy, None, address.port())
        };
        let url = proxy.url();
        // Drop any entry the policy already had under these names before
        // pushing: the executor renders env as a list, so leaving a duplicate
        // would make the effective value depend on ordering.
        let injected = ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "NO_PROXY"];
        policy
            .shell
            .env
            .retain(|variable| !injected.contains(&variable.name.as_str()));
        for name in ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"] {
            policy.shell.env.push(EnvVar {
                name: String::from(name),
                value: url.clone(),
            });
        }
        policy.shell.env.push(EnvVar {
            name: String::from("NO_PROXY"),
            value: String::from("127.0.0.1,localhost,::1"),
        });
        policy.network.proxy = Some(ProxyGrant {
            port,
            socket,
            egress: destinations,
        });
        Ok(proxy)
    }

    /// The loopback address the proxy listens on, when it is a TCP proxy; its
    /// port goes into the sandbox policy.
    #[must_use]
    pub const fn address(&self) -> Option<SocketAddr> {
        match &self.transport {
            Transport::Tcp(address) => Some(*address),
            Transport::Unix(_) => None,
        }
    }

    /// The Unix socket path the proxy listens on, when it is a Unix proxy.
    #[must_use]
    pub fn socket_path(&self) -> Option<&Path> {
        match &self.transport {
            Transport::Tcp(_) => None,
            Transport::Unix(path) => Some(path),
        }
    }

    /// The proxy URL to put in the child's environment, carrying the credential
    /// so a client sends it as `Proxy-Authorization`. The URL is kept out of
    /// logs; the token is per-proxy and dies with it.
    #[must_use]
    pub fn url(&self) -> String {
        match &self.transport {
            Transport::Tcp(address) => format!("http://agentd:{}@{address}", self.token),
            // A Unix proxy is reached through the child's own forwarder, which
            // listens on loopback; the URL names that loopback port, not the
            // socket.
            Transport::Unix(_) => {
                format!("http://agentd:{}@127.0.0.1:{FORWARD_PORT}", self.token)
            },
        }
    }

    /// Stops serving.
    pub fn stop(self) {
        let _ = self.shutdown.send(true);
        self.task.abort();
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        // An early return (e.g. building the session manager fails) drops the
        // handle without `stop`; stop the accept task anyway so it does not
        // outlive the caller's intent.
        let _ = self.shutdown.send(true);
        self.task.abort();
        // A Unix socket is a filesystem object: unlinking it here keeps the
        // runtime directory from filling with one file per session. A socket
        // another process has already replaced is left alone.
        if let Transport::Unix(path) = &self.transport
            && let Ok(metadata) = std::fs::metadata(path)
            && metadata.file_type().is_socket()
        {
            let _ = std::fs::remove_file(path);
        }
        // The proxy owns the directory it created for its socket; removing it
        // keeps `temp_dir` clean and cannot affect a socket it did not create.
        if let Transport::Unix(path) = &self.transport
            && let Some(parent) = path.parent()
            && parent
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("agentd-egress-"))
        {
            let _ = std::fs::remove_dir(parent);
        }
    }
}

/// Takes a connection permit, dropping the stream when at capacity. With no
/// bound, a local peer could open connections until the process runs out of
/// descriptors.
fn take_permit<S>(
    permits: &Arc<Semaphore>,
    _stream: &S,
) -> Option<tokio::sync::OwnedSemaphorePermit> {
    permits.clone().try_acquire_owned().ok()
}

/// Serves one client: authenticate, parse `CONNECT`, enforce the allowlist,
/// tunnel. Generic over the transport so TCP and Unix share one path.
async fn serve<S>(
    client: S,
    egress: &Egress,
    expected: &str,
) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    use tracing::Instrument as _;
    serve_inner(client, egress, expected)
        .instrument(tracing::info_span!("egress.proxy"))
        .await
}

/// The body of [`serve`], run inside the `egress.proxy` span so a child's trace
/// context has a span to attach its link to.
async fn serve_inner<S>(
    client: S,
    egress: &Egress,
    expected: &str,
) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    let (read_half, mut client_write) = tokio::io::split(client);
    // The reader is buffered and may hold bytes read past the CONNECT line, so
    // the tunnel copies through it rather than the raw read half.
    let mut client_read = BufReader::new(read_half);

    // The whole head is read under one timeout and one size cap, so a
    // pre-authentication client cannot stall the task or grow memory without
    // bound.
    let head = match tokio::time::timeout(HANDSHAKE_TIMEOUT, read_head(&mut client_read)).await {
        Ok(Ok(head)) => head,
        Ok(Err(error)) => return Err(error),
        Err(_) => return reply(&mut client_write, 408, "Request Timeout").await,
    };
    let Some((host, port)) = parse_connect(head.request_line()) else {
        return reply(&mut client_write, 400, "Bad Request").await;
    };
    // Correlate the request with the child's own span when it carried a trace
    // context, so the tunnel and any approval share its trace.
    if let Some(traceparent) = head
        .traceparent()
        .and_then(|value| Traceparent::parse(value).ok())
    {
        let _linked = crate::semconv::link_traceparent(
            &traceparent,
            vec![
                crate::semconv::Attribute::new("server.address", host.clone()),
                crate::semconv::Attribute::new("server.port", i64::from(port)),
            ],
        );
    }
    // Authenticate before revealing anything about the allowlist.
    if head.authorization() != Some(expected) {
        tracing::warn!(%host, %port, "the proxy refused an unauthenticated client");
        return reply_with(
            &mut client_write,
            407,
            "Proxy Authentication Required",
            "Proxy-Authenticate: Basic realm=\"agentd\"\r\n",
        )
        .await;
    }
    if !egress.allows(&host, port, head.traceparent()).await {
        tracing::warn!(%host, %port, "the proxy refused an egress destination");
        return reply(&mut client_write, 403, "Forbidden").await;
    }

    let upstream = match TcpStream::connect((host.as_str(), port)).await {
        Ok(upstream) => upstream,
        Err(error) => {
            tracing::warn!(%host, %port, %error, "the proxy could not reach the destination");
            return reply(&mut client_write, 502, "Bad Gateway").await;
        },
    };
    tracing::info!(%host, %port, "the proxy opened a tunnel");
    client_write
        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .await?;

    tunnel(client_read, client_write, upstream).await
}

/// Copies bytes in both directions until *both* end, honoring half-close: when
/// one side finishes writing it shuts its counterpart down, and the other
/// direction keeps flowing until it too ends.
async fn tunnel<R, W>(
    mut client_read: R,
    mut client_write: W,
    upstream: TcpStream,
) -> io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let (mut upstream_read, mut upstream_write) = upstream.into_split();
    let to_upstream = async {
        let result = tokio::io::copy(&mut client_read, &mut upstream_write).await;
        let _ = upstream_write.shutdown().await;
        result
    };
    let to_client = async {
        let result = tokio::io::copy(&mut upstream_read, &mut client_write).await;
        let _ = client_write.shutdown().await;
        result
    };
    let (upstream_done, client_done) = tokio::join!(to_upstream, to_client);
    upstream_done?;
    client_done?;
    Ok(())
}

/// The request head: the request line and the header lines, bounded in size.
struct Head {
    request_line: String,
    authorization: Option<String>,
    traceparent: Option<String>,
}

impl Head {
    /// The `CONNECT host:port HTTP/1.1` request line.
    fn request_line(&self) -> &str {
        &self.request_line
    }

    /// The `Proxy-Authorization` value, if any.
    fn authorization(&self) -> Option<&str> {
        self.authorization.as_deref()
    }

    /// The W3C `traceparent` the client sent, if any, so the proxy continues
    /// the caller's trace rather than starting an unrelated one.
    fn traceparent(&self) -> Option<&str> {
        self.traceparent.as_deref()
    }
}

/// Reads the request head, failing if it exceeds [`MAX_HEADER_BYTES`].
async fn read_head<R: AsyncBufRead + Unpin>(reader: &mut R) -> io::Result<Head> {
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).await? == 0 {
        return Ok(Head {
            request_line,
            authorization: None,
            traceparent: None,
        });
    }
    let mut authorization = None;
    let mut traceparent = None;
    let mut header = String::new();
    let mut total = request_line.len();
    loop {
        header.clear();
        if reader.read_line(&mut header).await? == 0 {
            break;
        }
        total += header.len();
        if total > MAX_HEADER_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the request head exceeds the limit",
            ));
        }
        let line = header.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("proxy-authorization") {
                authorization = Some(value.trim().to_string());
            } else if name.eq_ignore_ascii_case("traceparent") {
                traceparent = Some(value.trim().to_string());
            }
        }
    }
    Ok(Head {
        request_line,
        authorization,
        traceparent,
    })
}

/// Writes an empty HTTP response with `status` and no extra headers.
async fn reply<W: AsyncWrite + Unpin>(
    writer: &mut W,
    status: u16,
    reason: &str,
) -> io::Result<()> {
    reply_with(writer, status, reason, "").await
}

/// Writes an empty HTTP response with `status` and the given `extra` headers.
async fn reply_with<W: AsyncWrite + Unpin>(
    writer: &mut W,
    status: u16,
    reason: &str,
    extra: &str,
) -> io::Result<()> {
    writer
        .write_all(format!("HTTP/1.1 {status} {reason}\r\n{extra}\r\n").as_bytes())
        .await
}

/// The expected `Proxy-Authorization` header for `token` (`agentd:<token>`,
/// HTTP Basic).
fn basic_authorization(token: &str) -> String {
    let credentials = base64::engine::general_purpose::STANDARD.encode(format!("agentd:{token}"));
    format!("Basic {credentials}")
}

/// Parses `CONNECT host:port HTTP/1.1` into its destination, accepting an
/// IPv6 literal in brackets (`[::1]:443`).
fn parse_connect(request: &str) -> Option<(String, u16)> {
    let mut parts = request.split_whitespace();
    if !parts.next()?.eq_ignore_ascii_case("CONNECT") {
        return None;
    }
    parse_authority(parts.next()?)
}

/// Splits a host:port authority, honoring a bracketed IPv6 literal.
fn parse_authority(authority: &str) -> Option<(String, u16)> {
    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        let (host, port) = rest.split_once(']')?;
        (host, port.strip_prefix(':')?)
    } else {
        authority.rsplit_once(':')?
    };
    Some((host.to_string(), port.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::{
        EGRESS_DENIED, EGRESS_GRANTED, Egress, FORWARD_PORT, Proxy, basic_authorization,
        parse_connect,
    };
    use agentd_events::{Event, EventLog};
    use agentd_sandbox::{HostPort, Policy as SandboxPolicy};
    use serde_json::json;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::{TcpListener, TcpStream};

    /// The `HTTP_PROXY` value a policy injects, if any.
    fn proxy_env(policy: &SandboxPolicy) -> Option<&str> {
        policy
            .shell
            .env
            .iter()
            .find(|variable| variable.name == "HTTP_PROXY")
            .map(|variable| variable.value.as_str())
    }

    #[tokio::test]
    async fn a_namespace_host_grants_a_unix_socket_proxy() {
        let mut policy = SandboxPolicy::default();
        let proxy = Proxy::start_for_policy_with(
            &mut policy,
            Egress::new(vec![HostPort {
                host: String::from("api.example.com"),
                port: 443,
            }]),
            true,
        )
        .await
        .expect("the policy starts a Unix-socket proxy");

        // The child reaches the forwarder's loopback port, and the socket is
        // bind-mounted in, so the policy carries the socket.
        let grant = policy
            .network
            .proxy
            .as_ref()
            .expect("the policy grants the proxy");
        assert_eq!(grant.port, FORWARD_PORT);
        assert_eq!(grant.socket.as_deref(), proxy.socket_path());
        assert!(grant.socket.is_some());
        assert_eq!(
            proxy_env(&policy),
            Some(proxy.url().as_str()),
            "the proxy URL must be injected"
        );
        assert!(
            policy
                .shell
                .env
                .iter()
                .any(|variable| variable.name == "NO_PROXY"
                    && variable.value == "127.0.0.1,localhost,::1"),
            "NO_PROXY must keep the agent's own loopback off the tunnel"
        );

        proxy.stop();
    }

    #[tokio::test]
    async fn a_host_without_a_namespace_grants_a_loopback_tcp_proxy() {
        // The loopback-TCP form is the non-Linux path (macOS Seatbelt). Linux
        // has the namespace and must fail closed instead, so this branch is
        // exercised only where it is the real one.
        if cfg!(target_os = "linux") {
            eprintln!("skipping: Linux uses the private-namespace transport");
            return;
        }
        let mut policy = SandboxPolicy::default();
        let proxy = Proxy::start_for_policy_with(&mut policy, Egress::default(), false)
            .await
            .expect("the policy starts a loopback TCP proxy");

        let grant = policy
            .network
            .proxy
            .as_ref()
            .expect("the policy grants the proxy");
        assert!(grant.socket.is_none(), "no namespace means no Unix socket");
        let address = proxy.address().expect("a TCP proxy binds an address");
        assert_eq!(grant.port, address.port());
        assert_eq!(proxy_env(&policy), Some(proxy.url().as_str()));

        proxy.stop();
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn linux_without_a_namespace_fails_closed() {
        // Linux can express the strong form, where the port-only grant is
        // host-agnostic (Landlock has no address dimension), so egress must be
        // refused rather than silently weakened.
        let mut policy = SandboxPolicy::default();
        let Err(error) = Proxy::start_for_policy_with(&mut policy, Egress::default(), false).await
        else {
            panic!("Linux must refuse egress without a private namespace");
        };

        assert!(
            error.to_string().contains("bubblewrap"),
            "the failure must name bubblewrap: {error}"
        );
        assert!(
            policy.network.proxy.is_none(),
            "a refused proxy must leave the policy unwired"
        );
        assert!(
            proxy_env(&policy).is_none(),
            "a refused proxy must inject no proxy env"
        );
    }

    #[tokio::test]
    async fn an_existing_proxy_env_is_replaced_not_duplicated() {
        // The policy comes from an operator-written file, and the executor
        // renders its env verbatim, so a duplicate would make the effective
        // value depend on ordering.
        let mut policy = SandboxPolicy {
            shell: agentd_sandbox::ShellPolicy {
                env: vec![
                    agentd_sandbox::EnvVar {
                        name: String::from("HTTP_PROXY"),
                        value: String::from("http://stale.invalid:1"),
                    },
                    agentd_sandbox::EnvVar {
                        name: String::from("NO_PROXY"),
                        value: String::from("internal.corp"),
                    },
                ],
                ..agentd_sandbox::ShellPolicy::default()
            },
            ..SandboxPolicy::default()
        };
        let proxy = Proxy::start_for_policy_with(&mut policy, Egress::default(), true)
            .await
            .expect("the policy starts a Unix-socket proxy");

        for name in ["HTTP_PROXY", "NO_PROXY"] {
            let matches = policy
                .shell
                .env
                .iter()
                .filter(|variable| variable.name == name)
                .count();
            assert_eq!(matches, 1, "{name} must appear exactly once");
        }
        assert_eq!(
            proxy_env(&policy),
            Some(proxy.url().as_str()),
            "the injected URL must win over the operator's stale value"
        );
        assert!(
            !policy
                .shell
                .env
                .iter()
                .any(|variable| variable.value == "internal.corp"),
            "NO_PROXY must be replaced, not appended to"
        );

        proxy.stop();
    }

    #[tokio::test]
    async fn an_unlisted_destination_is_denied_without_an_approver() {
        let egress = Egress::default();
        assert!(!egress.allows("evil.example.com", 443, None).await);
    }

    #[tokio::test]
    async fn the_child_traceparent_reaches_the_approval_event() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = EventLog::open(dir.path().join("events.jsonl")).expect("log");
        let egress = Egress::default().with_approver(log.clone(), Duration::from_millis(50));

        let traceparent = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        let mut events = log.subscribe();
        // No approver answers: the request is recorded, then the deadline
        // cancels it.
        assert!(
            !egress
                .allows("api.example.com", 443, Some(traceparent))
                .await
        );

        let mut seen = Vec::new();
        while let Ok(entry) = events.try_recv() {
            seen.push(entry.event);
        }
        let requested = seen
            .iter()
            .find(|event| event.r#type == super::EGRESS_REQUESTED)
            .expect("an egress request must be recorded");
        assert_eq!(requested.traceparent.as_deref(), Some(traceparent));
        let cancelled = seen
            .iter()
            .find(|event| event.r#type == super::EGRESS_CANCELLED)
            .expect("a cancellation must be recorded");
        assert_eq!(cancelled.traceparent.as_deref(), Some(traceparent));
    }

    #[tokio::test]
    async fn an_approver_can_grant_an_unlisted_destination() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = EventLog::open(dir.path().join("events.jsonl")).expect("log");
        let egress = Egress::default().with_approver(log.clone(), Duration::from_secs(5));

        // An approver grants the request when it sees it.
        let mut events = log.subscribe();
        let decider = tokio::spawn(async move {
            while let Ok(entry) = events.recv().await {
                if entry.event.r#type == super::EGRESS_REQUESTED {
                    let request_id = entry.event.data["request_id"].clone();
                    log.publish(Event::new(
                        EGRESS_GRANTED,
                        json!({ "request_id": request_id }),
                    ))
                    .await
                    .expect("grant");
                    return;
                }
            }
        });

        assert!(egress.allows("api.example.com", 443, None).await);
        decider.await.expect("decider joins");
    }

    #[tokio::test]
    async fn a_denied_destination_stays_denied() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = EventLog::open(dir.path().join("events.jsonl")).expect("log");
        let egress = Egress::default().with_approver(log.clone(), Duration::from_secs(5));

        let mut events = log.subscribe();
        let decider = tokio::spawn(async move {
            while let Ok(entry) = events.recv().await {
                if entry.event.r#type == super::EGRESS_REQUESTED {
                    let request_id = entry.event.data["request_id"].clone();
                    log.publish(Event::new(
                        EGRESS_DENIED,
                        json!({ "request_id": request_id }),
                    ))
                    .await
                    .expect("deny");
                    return;
                }
            }
        });

        assert!(!egress.allows("evil.example.com", 443, None).await);
        decider.await.expect("decider joins");
    }

    #[tokio::test]
    async fn a_listed_destination_never_asks() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = EventLog::open(dir.path().join("events.jsonl")).expect("log");
        // A zero timeout: a listed destination must not consult the approver.
        let egress = Egress::new(vec![HostPort {
            host: String::from("api.example.com"),
            port: 443,
        }])
        .with_approver(log, Duration::from_millis(1));

        assert!(egress.allows("api.example.com", 443, None).await);
    }

    #[test]
    fn only_allowlisted_destinations_pass() {
        let egress = Egress::new(vec![HostPort {
            host: String::from("api.example.com"),
            port: 443,
        }]);

        assert!(egress.listed("api.example.com", 443));
        assert!(egress.listed("API.EXAMPLE.COM", 443));
        assert!(!egress.listed("api.example.com", 80));
        assert!(!egress.listed("evil.example.com", 443));
        assert!(!Egress::default().listed("api.example.com", 443));
    }

    #[test]
    fn connect_line_parses_the_authority() {
        assert_eq!(
            parse_connect("CONNECT api.example.com:443 HTTP/1.1\r\n"),
            Some((String::from("api.example.com"), 443))
        );
        assert_eq!(
            parse_connect("connect 127.0.0.1:9000 HTTP/1.1\r\n"),
            Some((String::from("127.0.0.1"), 9000))
        );
        assert_eq!(
            parse_connect("CONNECT [::1]:443 HTTP/1.1\r\n"),
            Some((String::from("::1"), 443))
        );
        assert_eq!(parse_connect("GET / HTTP/1.1\r\n"), None);
        assert_eq!(parse_connect("CONNECT no-port HTTP/1.1\r\n"), None);
        assert_eq!(parse_connect("CONNECT host:notaport HTTP/1.1\r\n"), None);
        assert_eq!(parse_connect("CONNECT [::1 HTTP/1.1\r\n"), None);
    }

    /// Opens a `CONNECT host:port` tunnel through `proxy` with its credential
    /// and returns the stream and the status line the proxy answered.
    async fn connect_through(
        proxy: &Proxy,
        host: &str,
        port: u16,
    ) -> std::io::Result<(TcpStream, String)> {
        connect_with_authorization(proxy, host, port, &basic_authorization(&proxy.token)).await
    }

    /// As [`connect_through`] but with an explicit `Proxy-Authorization` value.
    async fn connect_with_authorization(
        proxy: &Proxy,
        host: &str,
        port: u16,
        authorization: &str,
    ) -> std::io::Result<(TcpStream, String)> {
        let address = proxy.address().expect("a TCP proxy");
        let mut stream = TcpStream::connect(address).await?;
        stream
            .write_all(
                format!(
                    "CONNECT {host}:{port} HTTP/1.1\r\nProxy-Authorization: {authorization}\r\n\r\n"
                )
                .as_bytes(),
            )
            .await?;
        let mut status = Vec::new();
        let mut byte = [0_u8; 1];
        loop {
            let read = stream.read(&mut byte).await?;
            if read == 0 {
                break;
            }
            status.push(byte[0]);
            if status.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        Ok((stream, String::from_utf8_lossy(&status).into_owned()))
    }

    /// Starts an echo server and a proxy allowing it, returning both.
    async fn echo_and_proxy() -> (Proxy, u16) {
        let echo = TcpListener::bind("127.0.0.1:0").await.expect("echo binds");
        let echo_port = echo.local_addr().expect("addr").port();
        tokio::spawn(async move {
            let (mut socket, _) = echo.accept().await.expect("accept");
            let mut buffer = [0_u8; 64];
            let read = socket.read(&mut buffer).await.expect("read");
            socket.write_all(&buffer[..read]).await.expect("write");
        });
        let proxy = Proxy::start(Egress::new(vec![HostPort {
            host: String::from("127.0.0.1"),
            port: echo_port,
        }]))
        .await
        .expect("proxy starts");
        (proxy, echo_port)
    }

    #[tokio::test]
    async fn an_allowed_tunnel_answers_connection_established() {
        let (proxy, echo_port) = echo_and_proxy().await;

        let (_stream, status) = connect_through(&proxy, "127.0.0.1", echo_port)
            .await
            .expect("connect");
        assert!(status.starts_with("HTTP/1.1 200"), "status: {status}");

        proxy.stop();
    }

    #[tokio::test]
    async fn an_allowed_tunnel_carries_bytes_both_ways() {
        let (proxy, echo_port) = echo_and_proxy().await;

        let (mut stream, _status) = connect_through(&proxy, "127.0.0.1", echo_port)
            .await
            .expect("connect");
        stream.write_all(b"ping").await.expect("write");
        let mut echo = [0_u8; 4];
        stream.read_exact(&mut echo).await.expect("read");
        assert_eq!(&echo, b"ping");

        proxy.stop();
    }

    #[tokio::test]
    async fn a_denied_destination_is_refused_without_connecting() {
        let proxy = Proxy::start(Egress::default()).await.expect("proxy starts");

        let (_stream, status) = connect_through(&proxy, "api.example.com", 443)
            .await
            .expect("connect");
        assert!(status.starts_with("HTTP/1.1 403"), "status: {status}");

        proxy.stop();
    }

    #[tokio::test]
    async fn a_missing_or_wrong_credential_is_refused() {
        let proxy = Proxy::start(Egress::default()).await.expect("proxy starts");

        let (_stream, status) =
            connect_with_authorization(&proxy, "api.example.com", 443, "Basic wrong")
                .await
                .expect("connect");
        assert!(
            status.starts_with("HTTP/1.1 407"),
            "an unauthenticated client must be refused: {status}"
        );
        assert!(
            status.contains("Proxy-Authenticate"),
            "a 407 must advertise the scheme: {status}"
        );

        proxy.stop();
    }

    #[tokio::test]
    async fn the_tunnel_survives_a_client_half_close() {
        // The upstream answers only after it reads the request and sees EOF, so
        // the client must be able to shut down its write side and still read the
        // reply through the tunnel.
        let server = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("server binds");
        let server_port = server.local_addr().expect("addr").port();
        tokio::spawn(async move {
            let (mut socket, _) = server.accept().await.expect("accept");
            let mut buffer = [0_u8; 64];
            let read = socket.read(&mut buffer).await.expect("read");
            socket.write_all(&buffer[..read]).await.expect("write");
        });
        let proxy = Proxy::start(Egress::new(vec![HostPort {
            host: String::from("127.0.0.1"),
            port: server_port,
        }]))
        .await
        .expect("proxy starts");

        let (mut stream, status) = connect_through(&proxy, "127.0.0.1", server_port)
            .await
            .expect("connect");
        assert!(status.starts_with("HTTP/1.1 200"), "status: {status}");
        stream.write_all(b"ping").await.expect("write");
        stream.shutdown().await.expect("half-close");
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.expect("read reply");
        assert_eq!(response, b"ping", "the reply must survive the half-close");

        proxy.stop();
    }

    #[tokio::test]
    async fn a_namespace_proxy_owns_a_private_socket_directory() {
        // The socket lives in a private per-proxy directory rather than the
        // shared temp dir, so it is not exposed to every local user.
        let mut policy = SandboxPolicy::default();
        let proxy = Proxy::start_for_policy_with(&mut policy, Egress::default(), true)
            .await
            .expect("the policy starts a Unix-socket proxy");

        let socket = proxy.socket_path().expect("a Unix proxy has a path");
        let dir = socket.parent().expect("the socket has a parent");
        assert!(
            dir.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("agentd-egress-")),
            "the socket must live in its own directory: {dir:?}"
        );

        let dir = dir.to_path_buf();
        proxy.stop();

        assert!(
            !dir.exists(),
            "stopping the proxy must remove its socket directory: {dir:?}"
        );
    }

    #[tokio::test]
    async fn a_unix_proxy_removes_its_socket_when_it_stops() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("proxy.sock");
        let proxy = Proxy::start_unix(&path, Egress::default()).expect("unix proxy starts");
        assert!(path.exists(), "the socket must exist while the proxy runs");

        proxy.stop();

        assert!(
            !path.exists(),
            "stopping the proxy must unlink its socket, or the runtime directory \
             fills with one file per session"
        );
    }

    #[tokio::test]
    async fn a_unix_proxy_tunnels_to_an_allowed_destination() {
        use tokio::net::UnixStream;

        let echo = TcpListener::bind("127.0.0.1:0").await.expect("echo binds");
        let echo_port = echo.local_addr().expect("addr").port();
        tokio::spawn(async move {
            let (mut socket, _) = echo.accept().await.expect("accept");
            let mut buffer = [0_u8; 64];
            let read = socket.read(&mut buffer).await.expect("read");
            socket.write_all(&buffer[..read]).await.expect("write");
        });

        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("proxy.sock");
        let proxy = Proxy::start_unix(
            &socket_path,
            Egress::new(vec![HostPort {
                host: String::from("127.0.0.1"),
                port: echo_port,
            }]),
        )
        .expect("unix proxy starts");

        let mut stream = UnixStream::connect(&socket_path).await.expect("connect");
        stream
            .write_all(
                format!(
                    "CONNECT 127.0.0.1:{echo_port} HTTP/1.1\r\nProxy-Authorization: {}\r\n\r\n",
                    basic_authorization(&proxy.token)
                )
                .as_bytes(),
            )
            .await
            .expect("write head");
        let mut status = Vec::new();
        let mut byte = [0_u8; 1];
        loop {
            let read = stream.read(&mut byte).await.expect("read status");
            if read == 0 {
                break;
            }
            status.push(byte[0]);
            if status.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        assert!(
            String::from_utf8_lossy(&status).starts_with("HTTP/1.1 200"),
            "status: {}",
            String::from_utf8_lossy(&status)
        );
        stream.write_all(b"ping").await.expect("write");
        let mut echo = [0_u8; 4];
        stream.read_exact(&mut echo).await.expect("read");
        assert_eq!(&echo, b"ping");

        proxy.stop();
    }
}
