//! The transport: one `CONNECT`, one TLS handshake, one HTTP/1.1 exchange.
//!
//! The agent reaches its endpoint through the session's egress proxy, and the
//! proxy authenticates a `CONNECT` with `Proxy-Authorization: Bearer <token>`
//! (`sandbox::egress`). No off-the-shelf client sends that header: they either
//! turn the proxy URL's userinfo into `Basic`, or offer no way to set the header
//! at all. So the handshake is written out here, which also keeps this client's
//! dependencies to TLS alone.
//!
//! Everything is bounded, and every bound is enforced **while reading**: the
//! response head at [`MAX_HEAD_BYTES`], the body at [`MAX_REPLY_BYTES`], and the
//! whole exchange under one deadline. Reading first and checking afterwards is the
//! mistake that once OOM-killed this agent; see `docs/session/decision-trail.md`
//! row 14.
//!
//! Only `https` is accepted, for the reason the settings file refuses `http`: the
//! proxy speaks `CONNECT` alone, so a plaintext endpoint has no route, and the
//! tunnel is opaque so TLS stays end to end. See `docs/sandbox/network.md`.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Instant;

use rustls::pki_types::ServerName;
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};

use super::Error;

/// The most bytes of a response head this client reads.
///
/// A head is a few hundred bytes. This is far more than any provider sends, and it
/// bounds what a hostile or broken endpoint can make the client buffer before the
/// body even starts.
pub(super) const MAX_HEAD_BYTES: usize = 16 * 1024;

/// The most bytes of a reply body this client reads.
///
/// A turn of prose and a few tool calls do not approach a mebibyte. A reply larger
/// than this is refused rather than truncated: a cut JSON document is not a reply.
pub(super) const MAX_REPLY_BYTES: usize = 1024 * 1024;

/// The port an `https` URL without one reaches.
const HTTPS_PORT: u16 = 443;

/// The port an `http` proxy URL without one uses.
const HTTP_PORT: u16 = 80;

/// The largest response head this client will quote in an error.
const MAX_EXCERPT_BYTES: usize = 200;

/// Mozilla's roots, compiled into the binary.
///
/// The session's policy grants the agent its program directory and its workspace,
/// nothing else, so no CA file is readable inside the sandbox. The trust store
/// therefore comes from the binary rather than from the filesystem.
pub(super) fn compiled_roots() -> RootCertStore {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    roots
}

/// The proxy a request goes through.
///
/// The URL the sandbox injects is `http://agent:<token>@127.0.0.1:<port>`: the
/// token is the bearer value the proxy expects and the host is the child-side
/// forwarder on the command's own loopback. The token never reaches a log — see the
/// [`Debug`] impl — because it is the whole of the proxy's authorization.
#[derive(Clone, PartialEq, Eq)]
pub struct Proxy {
    /// The proxy's host, which this client resolves (the endpoint's is the
    /// proxy's business).
    host: String,
    /// The proxy's port.
    port: u16,
    /// The bearer value, when the URL carried one.
    token: Option<String>,
}

impl Proxy {
    /// Parse the proxy URL the sandbox injects.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ProxyUrl`] for a URL that is not an `http` proxy with a
    /// usable host and port. An `https` proxy is refused because the injected
    /// value never names one.
    pub fn parse(url: &str) -> Result<Self, Error> {
        let refuse = || Error::ProxyUrl(url.to_owned());
        let rest = url.strip_prefix("http://").ok_or_else(refuse)?;
        let (userinfo, authority) = match rest.rsplit_once('@') {
            Some((userinfo, authority)) => (Some(userinfo), authority),
            None => (None, rest),
        };
        let authority = authority.split('/').next().unwrap_or_default();
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) => (
                host,
                port.parse::<u16>()
                    .ok()
                    .filter(|port| *port != 0)
                    .ok_or_else(refuse)?,
            ),
            None => (authority, HTTP_PORT),
        };
        if host.is_empty() || host.contains(':') {
            return Err(refuse());
        }
        // `agent:<token>`: the username names the proxy, the password is the bearer
        // value. A URL with no password carries no token.
        let token = userinfo
            .and_then(|userinfo| userinfo.split_once(':'))
            .map(|(_, token)| token.to_owned());
        Ok(Self {
            host: host.to_ascii_lowercase(),
            port,
            token,
        })
    }

    /// The proxy's host.
    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The proxy's port.
    #[must_use]
    pub const fn port(&self) -> u16 {
        self.port
    }
}

impl std::fmt::Debug for Proxy {
    fn fmt(
        &self,
        f: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        f.debug_struct("Proxy")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// Where a request goes.
#[derive(Debug, Clone)]
pub(super) struct Target {
    /// The endpoint's host, which only the proxy resolves.
    pub(super) host: String,
    /// The endpoint's port.
    pub(super) port: u16,
    /// The request path, ending in `/chat/completions`.
    pub(super) path: String,
}

impl Target {
    /// The destination `base_url` names, with the completions path appended.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Endpoint`] for a base URL that is not an `https` URL with a
    /// usable host and no query or fragment.
    pub(super) fn parse(base_url: &str) -> Result<Self, Error> {
        let refuse = || Error::Endpoint(base_url.to_owned());
        let rest = base_url.strip_prefix("https://").ok_or_else(refuse)?;
        if rest.contains('?') || rest.contains('#') {
            return Err(refuse());
        }
        let (authority, path) = match rest.split_once('/') {
            Some((authority, path)) => (authority, format!("/{path}")),
            None => (rest, String::new()),
        };
        // Credentials belong in the environment, and a bracketed host could never
        // match a `CONNECT` target, which is matched without brackets.
        if authority.contains('@') || authority.contains('[') {
            return Err(refuse());
        }
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) => (
                host,
                port.parse::<u16>()
                    .ok()
                    .filter(|port| *port != 0)
                    .ok_or_else(refuse)?,
            ),
            None => (authority, HTTPS_PORT),
        };
        if host.is_empty() || host.contains(':') || !host.is_ascii() {
            return Err(refuse());
        }
        let base = path.trim_end_matches('/');
        Ok(Self {
            host: host.to_ascii_lowercase(),
            port,
            path: format!("{base}/chat/completions"),
        })
    }
}

/// A reply's status and body.
#[derive(Debug)]
pub(super) struct Reply {
    /// The HTTP status.
    pub(super) status: u16,
    /// The body, at most [`MAX_REPLY_BYTES`].
    pub(super) body: Vec<u8>,
}

/// Send `body` to `target` and read the reply, all under `deadline`.
///
/// # Errors
///
/// Returns [`Error`] naming the step that failed: the proxy refused the tunnel, the
/// endpoint could not be reached or trusted, the deadline passed, a head or body
/// exceeded its bound, or the head was malformed.
pub(super) fn exchange(
    tls: &Arc<ClientConfig>,
    target: &Target,
    proxy: Option<&Proxy>,
    authorization: &str,
    body: &[u8],
    deadline: Instant,
) -> Result<Reply, Error> {
    let socket = match proxy {
        Some(proxy) => connect_through(proxy, target, deadline)?,
        None => direct(target, deadline)?,
    };
    let name = ServerName::try_from(target.host.clone())
        .map_err(|error| Error::Endpoint(error.to_string()))?;
    let connection = ClientConnection::new(Arc::clone(tls), name)
        .map_err(|error| Error::Transport(std::io::Error::other(error)))?;
    let mut stream = StreamOwned::new(connection, socket);

    write_request(&mut stream, target, authorization, body)?;
    let (head, leftover) = read_head(&mut stream)?;
    let head = parse_head(&head)?;
    // The head read may have consumed the first bytes of the body, so the body is
    // read from what was left over before the socket.
    let mut source = Prefixed::new(&leftover, &mut stream);
    let body = read_body(&mut source, &head)?;
    Ok(Reply {
        status: head.status,
        body,
    })
}

/// Connect to the endpoint directly, which is what a client with no proxy does.
fn direct(
    target: &Target,
    deadline: Instant,
) -> Result<Tunnel, Error> {
    let socket = TcpStream::connect((target.host.as_str(), target.port)).map_err(io)?;
    Ok(Tunnel::new(socket, deadline))
}

/// Connect to the endpoint through `proxy`, which is the only route a session has.
fn connect_through(
    proxy: &Proxy,
    target: &Target,
    deadline: Instant,
) -> Result<Tunnel, Error> {
    let socket = TcpStream::connect((proxy.host(), proxy.port())).map_err(io)?;
    let mut tunnel = Tunnel::new(socket, deadline);
    let authority = format!("{}:{}", target.host, target.port);
    let authorization = proxy.token.as_ref().map_or_else(String::new, |token| {
        format!("Proxy-Authorization: Bearer {token}\r\n")
    });
    let request =
        format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n{authorization}\r\n");
    tunnel.write_all(request.as_bytes()).map_err(io)?;
    tunnel.flush().map_err(io)?;

    let (head, leftover) = read_head(&mut tunnel)?;
    let head = parse_head(&head)?;
    if head.status != 200 {
        return Err(Error::Proxy {
            status: head.status,
            reason: reason(&leftover),
        });
    }
    // The proxy answers the head and then tunnels. Anything it sent past the head
    // is the first bytes of the tunnel, so it is kept for TLS rather than dropped.
    tunnel.prefix = leftover;
    tunnel.offset = 0;
    Ok(tunnel)
}

/// Write the request head and body.
fn write_request(
    stream: &mut impl Write,
    target: &Target,
    authorization: &str,
    body: &[u8],
) -> Result<(), Error> {
    let authority = if target.port == HTTPS_PORT {
        target.host.clone()
    } else {
        format!("{}:{}", target.host, target.port)
    };
    let head = format!(
        "POST {path} HTTP/1.1\r\nHost: {authority}\r\nAuthorization: {authorization}\r\n\
Content-Type: application/json\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n",
        path = target.path,
        length = body.len(),
    );
    stream.write_all(head.as_bytes()).map_err(io)?;
    stream.write_all(body).map_err(io)?;
    stream.flush().map_err(io)
}

/// Read one head, up to and including its blank line, and what arrived past it.
///
/// # Errors
///
/// Returns [`Error::TooLarge`] when the head exceeds [`MAX_HEAD_BYTES`], and
/// [`Error::Reply`] when the connection ends before the blank line arrives.
fn read_head(reader: &mut impl Read) -> Result<(Vec<u8>, Vec<u8>), Error> {
    let mut head: Vec<u8> = Vec::new();
    let mut buffer = [0_u8; 512];
    loop {
        if let Some(end) = head_end(&head) {
            let leftover = head.split_off(end);
            return Ok((head, leftover));
        }
        if head.len() >= MAX_HEAD_BYTES {
            return Err(Error::TooLarge {
                what: "head",
                limit: MAX_HEAD_BYTES,
            });
        }
        let read = reader.read(&mut buffer).map_err(io)?;
        if read == 0 {
            return Err(Error::Reply(
                "the connection ended before a head arrived".to_owned(),
            ));
        }
        head.extend_from_slice(&buffer[..read]);
    }
}

/// The index just past the head's terminating blank line.
///
/// The whole buffer is searched each time more arrives. A head is bounded by
/// [`MAX_HEAD_BYTES`], so the repeated scan costs nothing worth avoiding, and it
/// cannot miss a terminator that straddles two reads.
fn head_end(bytes: &[u8]) -> Option<usize> {
    for index in 0..bytes.len() {
        if bytes[index..].starts_with(b"\r\n\r\n") {
            return Some(index + 4);
        }
        // A bare LF is accepted: the sandbox's own proxy tolerates it.
        if bytes[index] == b'\n' && bytes.get(index + 1) == Some(&b'\n') {
            return Some(index + 2);
        }
    }
    None
}

/// A response head's status and the framing of the body that follows it.
#[derive(Debug)]
struct Head {
    /// The status code.
    status: u16,
    /// Whether the body is chunked.
    chunked: bool,
    /// The declared body length, when the head names one.
    length: Option<usize>,
}

/// Parse a response head.
///
/// # Errors
///
/// Returns [`Error::Reply`] for a head whose status line is not HTTP or whose
/// `Content-Length` is not a number.
fn parse_head(head: &[u8]) -> Result<Head, Error> {
    let text = String::from_utf8_lossy(head);
    let mut lines = text.split('\n');
    let status_line = lines
        .next()
        .ok_or_else(|| Error::Reply("the head is empty".to_owned()))?;
    let mut fields = status_line.split_whitespace();
    let version = fields.next().unwrap_or_default();
    let status = fields
        .next()
        .and_then(|code| code.parse::<u16>().ok())
        .filter(|_| version.starts_with("HTTP/"))
        .ok_or_else(|| Error::Reply(format!("the status line is not HTTP: {status_line:?}")))?;

    let mut parsed = Head {
        status,
        chunked: false,
        length: None,
    };
    for line in lines {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(Error::Reply(format!(
                "the head has a malformed line: {line:?}"
            )));
        };
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim();
        match name.as_str() {
            "transfer-encoding" if value.to_ascii_lowercase().contains("chunked") => {
                parsed.chunked = true;
            },
            "content-length" => {
                parsed.length = Some(value.parse::<usize>().map_err(|_| {
                    Error::Reply(format!(
                        "the head's content length is not a number: {value:?}"
                    ))
                })?);
            },
            _ => {},
        }
    }
    Ok(parsed)
}

/// Read the body a head describes, under [`MAX_REPLY_BYTES`].
///
/// # Errors
///
/// Returns [`Error::TooLarge`] when the body exceeds the cap, and [`Error::Reply`]
/// when the framing is broken or the body ends early.
fn read_body(
    reader: &mut impl Read,
    head: &Head,
) -> Result<Vec<u8>, Error> {
    if head.chunked {
        return read_chunked(reader);
    }
    if let Some(length) = head.length {
        if length > MAX_REPLY_BYTES {
            return Err(Error::TooLarge {
                what: "reply",
                limit: MAX_REPLY_BYTES,
            });
        }
        let mut body = Vec::with_capacity(length);
        reader
            .take(length as u64)
            .read_to_end(&mut body)
            .map_err(io)?;
        if body.len() != length {
            return Err(Error::Reply(format!(
                "the reply ended after {} of {length} bytes",
                body.len()
            )));
        }
        return Ok(body);
    }
    read_to_eof(reader)
}

/// Read a chunked body.
fn read_chunked(reader: &mut impl Read) -> Result<Vec<u8>, Error> {
    let mut body: Vec<u8> = Vec::new();
    loop {
        let size_line = read_line(reader)?;
        let size = size_line.split(';').next().unwrap_or_default().trim();
        let size = usize::from_str_radix(size, 16)
            .map_err(|_| Error::Reply(format!("the chunk size is not hexadecimal: {size:?}")))?;
        if size == 0 {
            // The trailers end at a blank line; they carry nothing this reads.
            loop {
                if read_line(reader)?.is_empty() {
                    return Ok(body);
                }
            }
        }
        if body.len() + size > MAX_REPLY_BYTES {
            return Err(Error::TooLarge {
                what: "reply",
                limit: MAX_REPLY_BYTES,
            });
        }
        let mut chunk = vec![0_u8; size];
        reader.read_exact(&mut chunk).map_err(io)?;
        body.extend_from_slice(&chunk);
        // The chunk's data is followed by a line break of its own.
        if !read_line(reader)?.is_empty() {
            return Err(Error::Reply(
                "a chunk was not followed by its line break".to_owned(),
            ));
        }
    }
}

/// Read a body that runs to the end of the connection.
fn read_to_eof(reader: &mut impl Read) -> Result<Vec<u8>, Error> {
    let mut body = Vec::new();
    reader
        .take(MAX_REPLY_BYTES as u64 + 1)
        .read_to_end(&mut body)
        .map_err(io)?;
    if body.len() > MAX_REPLY_BYTES {
        return Err(Error::TooLarge {
            what: "reply",
            limit: MAX_REPLY_BYTES,
        });
    }
    Ok(body)
}

/// Read one line, without its terminator, bounded by the head cap.
fn read_line(reader: &mut impl Read) -> Result<String, Error> {
    let mut line = Vec::new();
    let mut byte = [0_u8; 1];
    loop {
        if line.len() >= MAX_HEAD_BYTES {
            return Err(Error::TooLarge {
                what: "head",
                limit: MAX_HEAD_BYTES,
            });
        }
        let read = reader.read(&mut byte).map_err(io)?;
        if read == 0 || byte[0] == b'\n' {
            return Ok(String::from_utf8_lossy(&line)
                .trim_end_matches('\r')
                .to_owned());
        }
        line.push(byte[0]);
    }
}

/// The IO error as this module's error, with a read timeout named as what it is.
///
/// Every socket this module owns has its timeout set to the time left before the
/// deadline, so a timeout means the deadline passed rather than that the endpoint
/// paused.
fn io(error: std::io::Error) -> Error {
    if matches!(
        error.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    ) {
        return Error::Deadline;
    }
    Error::from(error)
}

/// Why a rejected reply was rejected: the provider's own message when it sent one,
/// otherwise a bounded excerpt of what arrived.
///
/// The bodies this reads are not only a provider's: the egress proxy answers `407`
/// and `403` in plain text, and the agent's report should say what it actually got.
pub(super) fn reason(body: &[u8]) -> String {
    if let Ok(reply) = serde_json::from_slice::<super::wire::Reply>(body)
        && let Some(message) = reply.error_reason()
    {
        return message;
    }
    if body.is_empty() {
        return "no reason was given".to_owned();
    }
    String::from_utf8_lossy(&body[..body.len().min(MAX_EXCERPT_BYTES)])
        .trim()
        .to_owned()
}

/// A socket that carries the bytes read past the `CONNECT` head, and enforces the
/// deadline on every read and write.
///
/// The handshake reads the proxy's reply from the socket, and one `read` may return
/// bytes beyond the head. They are the first bytes of the tunnel, so they are kept
/// here and handed to TLS rather than dropped. The bound lives in the type, so the
/// handshake, the request, the head, and the body are all under the same deadline
/// without each of them remembering to set it.
struct Tunnel {
    /// Bytes already read from the socket that belong to the tunnel.
    prefix: Vec<u8>,
    /// How far into `prefix` the reader has gone.
    offset: usize,
    /// The socket underneath.
    socket: TcpStream,
    /// When the exchange must be finished.
    deadline: Instant,
}

impl Tunnel {
    /// A tunnel over `socket` that must finish by `deadline`.
    const fn new(
        socket: TcpStream,
        deadline: Instant,
    ) -> Self {
        Self {
            prefix: Vec::new(),
            offset: 0,
            socket,
            deadline,
        }
    }

    /// Set the socket's timeout to the time left.
    fn bound(&self) -> Result<(), Error> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(Error::Deadline);
        }
        self.socket.set_read_timeout(Some(remaining)).map_err(io)?;
        self.socket.set_write_timeout(Some(remaining)).map_err(io)
    }
}

impl Read for Tunnel {
    fn read(
        &mut self,
        buffer: &mut [u8],
    ) -> std::io::Result<usize> {
        if self.offset < self.prefix.len() {
            let count = (self.prefix.len() - self.offset).min(buffer.len());
            buffer[..count].copy_from_slice(&self.prefix[self.offset..self.offset + count]);
            self.offset += count;
            return Ok(count);
        }
        self.bound().map_err(as_io)?;
        self.socket.read(buffer)
    }
}

impl Write for Tunnel {
    fn write(
        &mut self,
        buffer: &[u8],
    ) -> std::io::Result<usize> {
        self.bound().map_err(as_io)?;
        self.socket.write(buffer)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.bound().map_err(as_io)?;
        self.socket.flush()
    }
}

/// A reader that yields `prefix` before it reads `inner`.
///
/// The response head and the body arrive on one stream, and reading the head may
/// consume body bytes. This hands those bytes to the body reader first, so no
/// framing is lost at the seam.
struct Prefixed<'a, R> {
    /// The bytes already read past the head.
    prefix: &'a [u8],
    /// How far into `prefix` the reader has gone.
    offset: usize,
    /// The reader underneath.
    inner: R,
}

impl<'a, R> Prefixed<'a, R> {
    /// A reader over `inner` that starts with `prefix`.
    const fn new(
        prefix: &'a [u8],
        inner: R,
    ) -> Self {
        Self {
            prefix,
            offset: 0,
            inner,
        }
    }
}

impl<R: Read> Read for Prefixed<'_, R> {
    fn read(
        &mut self,
        buffer: &mut [u8],
    ) -> std::io::Result<usize> {
        if self.offset < self.prefix.len() {
            let count = (self.prefix.len() - self.offset).min(buffer.len());
            buffer[..count].copy_from_slice(&self.prefix[self.offset..self.offset + count]);
            self.offset += count;
            return Ok(count);
        }
        self.inner.read(buffer)
    }
}

/// This module's error as an IO error, for the [`Read`]/[`Write`] adapters.
fn as_io(error: Error) -> std::io::Error {
    std::io::Error::other(error)
}

#[cfg(test)]
mod tests {
    // Tests for the pieces that need no socket: the two URL parsers, the head
    // boundary (including a terminator split across two reads), the head parser,
    // the chunked reader, and the error mapping.

    use std::io::Cursor;
    use std::time::Duration;

    use super::*;

    /// A connected TCP pair, for driving a [`Tunnel`] without TLS.
    fn tcp_pair(deadline: Instant) -> (TcpStream, Tunnel) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("binds");
        let address = listener.local_addr().expect("has an address");
        let peer = TcpStream::connect(address).expect("connects");
        let (socket, _) = listener.accept().expect("accepts");
        (peer, Tunnel::new(socket, deadline))
    }

    #[test]
    fn a_proxy_url_from_the_sandbox_is_parsed() {
        let proxy = Proxy::parse("http://agent:tok@127.0.0.1:8080").expect("parses");
        assert_eq!(proxy.host(), "127.0.0.1");
        assert_eq!(proxy.port(), 8080);
    }

    #[test]
    fn a_proxy_url_without_a_token_carries_none() {
        let proxy = Proxy::parse("http://127.0.0.1:8080").expect("parses");
        assert!(proxy.token.is_none());
    }

    #[test]
    fn a_proxy_url_is_refused_when_it_is_not_a_usable_http_proxy() {
        for url in [
            "",
            "127.0.0.1:8080",
            "https://agent:tok@127.0.0.1:8080",
            "http://:8080",
            "http://agent:tok@127.0.0.1:0",
            "http://agent:tok@127.0.0.1:port",
        ] {
            let error = Proxy::parse(url).expect_err("refused");
            assert!(matches!(error, Error::ProxyUrl(_)), "`{url}` gave: {error}");
        }
    }

    #[test]
    fn a_proxy_url_without_a_port_uses_the_http_default() {
        assert_eq!(
            Proxy::parse("http://127.0.0.1").expect("parses").port(),
            HTTP_PORT
        );
    }

    #[test]
    fn a_proxys_debug_never_shows_its_token() {
        // The token is the whole of the proxy's authorization, and Debug reaches
        // logs.
        let proxy = Proxy::parse("http://agent:s3cr3t@127.0.0.1:8080").expect("parses");
        let debug = format!("{proxy:?}");
        assert!(!debug.contains("s3cr3t"), "debug was: {debug}");
        assert!(debug.contains("redacted"), "debug was: {debug}");
    }

    #[test]
    fn a_base_url_becomes_a_host_port_and_request_path() {
        let target = Target::parse("https://api.x.ai/v1").expect("parses");
        assert_eq!(target.host, "api.x.ai");
        assert_eq!(target.port, HTTPS_PORT);
        assert_eq!(target.path, "/v1/chat/completions");
    }

    #[test]
    fn a_base_url_without_a_path_gets_the_completions_path() {
        let target = Target::parse("https://api.x.ai").expect("parses");
        assert_eq!(target.path, "/chat/completions");
    }

    #[test]
    fn a_trailing_slash_does_not_double_the_separator() {
        let target = Target::parse("https://api.x.ai/v1/").expect("parses");
        assert_eq!(target.path, "/v1/chat/completions");
    }

    #[test]
    fn an_explicit_port_is_kept_and_the_host_is_folded() {
        let target = Target::parse("https://API.Example.COM:8443/v1").expect("parses");
        assert_eq!(target.host, "api.example.com");
        assert_eq!(target.port, 8443);
    }

    #[test]
    fn a_base_url_a_session_could_not_use_is_refused() {
        for base_url in [
            "",
            "api.x.ai/v1",
            "http://api.x.ai/v1",
            "https://key@api.x.ai/v1",
            "https:///v1",
            "https://[::1]:8443/v1",
            "https://апи.пример/v1",
            "https://api.x.ai:0/v1",
            "https://api.x.ai:port/v1",
            "https://api.x.ai/v1?key=1",
            "https://api.x.ai/v1#frag",
        ] {
            let error = Target::parse(base_url).expect_err("refused");
            assert!(
                matches!(error, Error::Endpoint(_)),
                "`{base_url}` gave: {error}"
            );
        }
    }

    #[test]
    fn a_head_is_found_whole() {
        assert_eq!(head_end(b"HTTP/1.1 200 OK\r\n\r\n"), Some(19));
    }

    #[test]
    fn a_head_ending_in_a_bare_line_feed_is_found() {
        assert_eq!(head_end(b"HTTP/1.1 200 OK\n\n"), Some(17));
    }

    #[test]
    fn a_head_terminator_split_across_reads_is_still_found() {
        // The search restarts before what it already searched, so the terminator
        // is found when the second half arrives.
        let bytes = b"HTTP/1.1 200 OK\r\n\r\n";
        assert_eq!(head_end(&bytes[..16]), None, "not complete yet");
        assert_eq!(head_end(bytes), Some(19));
    }

    #[test]
    fn a_head_that_never_ends_is_not_found() {
        assert_eq!(head_end(b"HTTP/1.1 200 OK\r\n"), None);
    }

    #[test]
    fn a_head_of_a_few_kibibytes_is_read() {
        // A head is normally a few hundred bytes, but the cap is a kibibyte count
        // rather than a byte count: a provider that pads its head is still read.
        let head = format!("HTTP/1.1 200 OK\r\nX-Padding: {}\r\n\r\n", "y".repeat(2048));
        let (read, leftover) = read_head(&mut Cursor::new(head.as_bytes())).expect("reads");
        assert_eq!(read, head.as_bytes());
        assert!(leftover.is_empty());
    }

    #[test]
    fn a_head_over_the_cap_is_refused() {
        let head = format!(
            "HTTP/1.1 200 OK\r\nX-Padding: {}\r\n\r\n",
            "y".repeat(MAX_HEAD_BYTES)
        );
        let error = read_head(&mut Cursor::new(head.as_bytes())).expect_err("refused");
        assert!(
            matches!(error, Error::TooLarge { what: "head", .. }),
            "gave: {error}"
        );
    }

    #[test]
    fn a_head_carries_the_bytes_that_arrived_past_it() {
        let (read, leftover) =
            read_head(&mut Cursor::new(b"HTTP/1.1 200 OK\r\n\r\nbody")).expect("reads");
        assert_eq!(read, b"HTTP/1.1 200 OK\r\n\r\n");
        assert_eq!(leftover, b"body", "the body after the head is kept");
    }

    #[test]
    fn a_status_and_its_framing_are_parsed() {
        let head = parse_head(b"HTTP/1.1 404 Not Found\r\nContent-Length: 12\r\nX: y\r\n\r\n")
            .expect("parses");
        assert_eq!(head.status, 404);
        assert_eq!(head.length, Some(12));
        assert!(!head.chunked);
    }

    #[test]
    fn header_names_are_matched_without_regard_to_case() {
        let head = parse_head(b"HTTP/1.1 200 OK\r\nCONTENT-LENGTH: 3\r\n\r\n").expect("parses");
        assert_eq!(head.length, Some(3));
        let head =
            parse_head(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: Chunked\r\n\r\n").expect("parses");
        assert!(head.chunked);
    }

    #[test]
    fn a_transfer_encoding_that_is_not_chunked_leaves_the_length_alone() {
        // Only `chunked` changes how the body is framed; any other value leaves the
        // declared length to say it.
        let head = parse_head(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: identity\r\nContent-Length: 3\r\n\r\n",
        )
        .expect("parses");
        assert!(!head.chunked);
        assert_eq!(head.length, Some(3));
    }

    #[test]
    fn a_head_without_framing_reads_to_the_end() {
        let head = parse_head(b"HTTP/1.1 200 OK\r\n\r\n").expect("parses");
        assert_eq!(head.length, None);
        assert!(!head.chunked);
    }

    #[test]
    fn a_head_that_is_not_http_is_refused() {
        for head in [&b""[..], b"garbage\r\n\r\n", b"HTTP/1.1 OK\r\n\r\n"] {
            let error = parse_head(head).expect_err("refused");
            assert!(matches!(error, Error::Reply(_)), "gave: {error}");
        }
    }

    #[test]
    fn a_malformed_head_line_is_refused() {
        let error = parse_head(b"HTTP/1.1 200 OK\r\nnot a header\r\n\r\n").expect_err("refused");
        assert!(matches!(error, Error::Reply(_)), "gave: {error}");
    }

    #[test]
    fn a_content_length_that_is_not_a_number_is_refused() {
        let error =
            parse_head(b"HTTP/1.1 200 OK\r\nContent-Length: twelve\r\n\r\n").expect_err("refused");
        assert!(matches!(error, Error::Reply(_)), "gave: {error}");
    }

    #[test]
    fn a_body_of_a_declared_length_is_read() {
        let head = Head {
            status: 200,
            chunked: false,
            length: Some(5),
        };
        let body = read_body(&mut Cursor::new(b"hello world"), &head).expect("reads");
        assert_eq!(body, b"hello");
    }

    #[test]
    fn a_body_that_ends_early_is_refused() {
        let head = Head {
            status: 200,
            chunked: false,
            length: Some(9),
        };
        let error = read_body(&mut Cursor::new(b"hello"), &head).expect_err("refused");
        assert!(matches!(error, Error::Reply(_)), "gave: {error}");
    }

    #[test]
    fn a_body_of_a_few_kibibytes_is_read() {
        // The cap is a kibibyte count rather than a byte count: a reply of a few
        // kibibytes is ordinary and is read in full.
        let head = Head {
            status: 200,
            chunked: false,
            length: None,
        };
        let read = read_body(&mut Cursor::new(vec![b'x'; 4096]), &head).expect("reads");
        assert_eq!(read.len(), 4096);
    }

    #[test]
    fn a_body_of_exactly_the_cap_is_read() {
        // The cap is a bound, not a limit below it.
        let head = Head {
            status: 200,
            chunked: false,
            length: Some(MAX_REPLY_BYTES),
        };
        let read = read_body(&mut Cursor::new(vec![b'x'; MAX_REPLY_BYTES]), &head).expect("reads");
        assert_eq!(read.len(), MAX_REPLY_BYTES);
    }

    #[test]
    fn an_unframed_body_of_exactly_the_cap_is_read() {
        let head = Head {
            status: 200,
            chunked: false,
            length: None,
        };
        let read = read_body(&mut Cursor::new(vec![b'x'; MAX_REPLY_BYTES]), &head).expect("reads");
        assert_eq!(read.len(), MAX_REPLY_BYTES);
    }

    #[test]
    fn a_chunked_body_of_exactly_the_cap_is_read() {
        let head = Head {
            status: 200,
            chunked: true,
            length: None,
        };
        let mut wire = format!("{MAX_REPLY_BYTES:x}\r\n").into_bytes();
        wire.extend(std::iter::repeat_n(b'x', MAX_REPLY_BYTES));
        wire.extend_from_slice(b"\r\n0\r\n\r\n");
        let read = read_body(&mut Cursor::new(wire), &head).expect("reads");
        assert_eq!(read.len(), MAX_REPLY_BYTES);
    }

    #[test]
    fn a_declared_length_over_the_cap_is_refused_before_reading() {
        // The cap is checked from the declared length, so a hostile length cannot
        // make the client allocate it.
        let head = Head {
            status: 200,
            chunked: false,
            length: Some(MAX_REPLY_BYTES * 8),
        };
        let error = read_body(&mut Cursor::new(Vec::new()), &head).expect_err("refused");
        assert!(
            matches!(error, Error::TooLarge { what: "reply", .. }),
            "gave: {error}"
        );
    }

    #[test]
    fn a_body_without_framing_is_read_to_the_end() {
        let head = Head {
            status: 200,
            chunked: false,
            length: None,
        };
        let body = read_body(&mut Cursor::new(b"{\"a\":1}"), &head).expect("reads");
        assert_eq!(body, b"{\"a\":1}");
    }

    #[test]
    fn a_body_over_the_cap_is_refused() {
        let head = Head {
            status: 200,
            chunked: false,
            length: None,
        };
        let oversized = vec![b'x'; MAX_REPLY_BYTES + 1];
        let error = read_body(&mut Cursor::new(oversized), &head).expect_err("refused");
        assert!(
            matches!(error, Error::TooLarge { what: "reply", .. }),
            "gave: {error}"
        );
    }

    #[test]
    fn a_chunked_body_is_decoded() {
        let head = Head {
            status: 200,
            chunked: true,
            length: None,
        };
        let body = read_body(
            &mut Cursor::new(b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n"),
            &head,
        )
        .expect("reads");
        assert_eq!(body, b"hello world");
    }

    #[test]
    fn a_chunked_body_with_a_trailer_is_decoded() {
        let head = Head {
            status: 200,
            chunked: true,
            length: None,
        };
        let body = read_body(
            &mut Cursor::new(b"2\r\nhi\r\n0\r\nX-Checksum: 1\r\n\r\n"),
            &head,
        )
        .expect("reads");
        assert_eq!(body, b"hi");
    }

    #[test]
    fn a_chunk_size_that_is_not_hexadecimal_is_refused() {
        let head = Head {
            status: 200,
            chunked: true,
            length: None,
        };
        let error =
            read_body(&mut Cursor::new(b"zz\r\nhi\r\n0\r\n\r\n"), &head).expect_err("refused");
        assert!(matches!(error, Error::Reply(_)), "gave: {error}");
    }

    #[test]
    fn a_chunk_without_its_line_break_is_refused() {
        let head = Head {
            status: 200,
            chunked: true,
            length: None,
        };
        let error =
            read_body(&mut Cursor::new(b"2\r\nhiX\r\n0\r\n\r\n"), &head).expect_err("refused");
        assert!(matches!(error, Error::Reply(_)), "gave: {error}");
    }

    #[test]
    fn a_chunked_body_over_the_cap_is_refused() {
        let head = Head {
            status: 200,
            chunked: true,
            length: None,
        };
        let size = format!("{MAX_REPLY_BYTES:x}");
        let mut body = Vec::new();
        body.extend_from_slice(size.as_bytes());
        body.extend_from_slice(b"\r\n");
        body.extend(std::iter::repeat_n(b'x', MAX_REPLY_BYTES));
        body.extend_from_slice(b"\r\n1\r\nx\r\n0\r\n\r\n");
        let error = read_body(&mut Cursor::new(body), &head).expect_err("refused");
        assert!(
            matches!(error, Error::TooLarge { what: "reply", .. }),
            "gave: {error}"
        );
    }

    #[test]
    fn a_read_timeout_is_reported_as_the_deadline() {
        for kind in [std::io::ErrorKind::WouldBlock, std::io::ErrorKind::TimedOut] {
            let error = io(std::io::Error::from(kind));
            assert!(matches!(error, Error::Deadline), "gave: {error}");
        }
    }

    #[test]
    fn another_io_error_is_reported_as_a_transport_failure() {
        let error = io(std::io::Error::from(std::io::ErrorKind::ConnectionReset));
        assert!(matches!(error, Error::Transport(_)), "gave: {error}");
    }

    #[test]
    fn a_prefixed_reader_yields_the_leftover_before_its_inner_reader() {
        // Read in pieces, so the leftover is consumed with a nonzero offset and
        // then the reader underneath takes over.
        let mut reader = Prefixed::new(b"leftover", Cursor::new(b"XY"));
        let mut first = [0_u8; 4];
        reader.read_exact(&mut first).expect("reads the first half");
        assert_eq!(&first, b"left", "the leftover comes first");
        let mut second = [0_u8; 6];
        reader.read_exact(&mut second).expect("reads the rest");
        assert_eq!(&second, b"overXY", "then the reader underneath");
    }

    #[test]
    fn a_tunnel_yields_the_bytes_read_past_the_head_before_the_socket() {
        // The `CONNECT` handshake may read past the proxy's head, and those bytes
        // are the tunnel's: they must reach TLS rather than be dropped.
        let (mut peer, mut tunnel) = tcp_pair(Instant::now() + Duration::from_secs(5));
        tunnel.prefix = b"leftover".to_vec();
        peer.write_all(b"XY").expect("writes to the tunnel");
        peer.flush().expect("flushes");

        let mut first = [0_u8; 4];
        tunnel.read_exact(&mut first).expect("reads the first half");
        assert_eq!(&first, b"left", "the leftover comes first");

        let mut second = [0_u8; 6];
        tunnel.read_exact(&mut second).expect("reads the rest");
        assert_eq!(&second, b"overXY", "then the socket underneath");
    }

    #[test]
    fn a_tunnel_writes_through_to_its_socket() {
        let (mut peer, mut tunnel) = tcp_pair(Instant::now() + Duration::from_secs(5));
        tunnel.write_all(b"out").expect("writes");
        tunnel.flush().expect("flushes");
        let mut read = [0_u8; 3];
        peer.read_exact(&mut read).expect("reads what was written");
        assert_eq!(&read, b"out");
    }

    #[test]
    fn the_roots_are_compiled_in() {
        assert!(
            compiled_roots().len() > 100,
            "a trust store that is not empty"
        );
    }

    #[test]
    fn a_rejected_reply_is_quoted_by_its_own_message() {
        assert_eq!(
            reason(br#"{"error": {"message": "invalid api key"}}"#),
            "invalid api key"
        );
    }

    #[test]
    fn a_rejected_reply_that_is_not_json_is_quoted_as_it_arrived() {
        // The egress proxy answers `407` with plain text, so the agent reports what
        // it actually got rather than "no reason".
        assert_eq!(
            reason(b"Proxy Authentication Required"),
            "Proxy Authentication Required"
        );
    }

    #[test]
    fn a_rejected_reply_says_when_it_carried_nothing() {
        assert_eq!(reason(b""), "no reason was given");
    }

    #[test]
    fn a_quoted_reason_is_bounded() {
        assert_eq!(
            reason(&vec![b'x'; MAX_EXCERPT_BYTES * 4]).len(),
            MAX_EXCERPT_BYTES
        );
    }
}
