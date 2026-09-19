//! The daemon's CONNECT proxy: the single egress path a confined command may
//! use. The design and trust model are in `docs/egress.md`.
//!
//! It requires a per-proxy credential so only the sandbox it was started for
//! can use it, not any other process of the same user; it does not terminate
//! TLS, so the provider credentials stay end to end.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use base64::Engine as _;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::{TcpListener, TcpStream};

use agentd_sandbox::HostPort;

/// The proxy allowlist: the `host:port` destinations a tunnel may open.
#[derive(Clone, Debug, Default)]
pub struct Egress {
    allowed: Arc<Vec<HostPort>>,
}

impl Egress {
    /// Builds an allowlist from `destinations`.
    #[must_use]
    pub fn new(destinations: Vec<HostPort>) -> Self {
        Self {
            allowed: Arc::new(destinations),
        }
    }

    /// Whether a tunnel to `host:port` is permitted.
    #[must_use]
    pub fn allows(
        &self,
        host: &str,
        port: u16,
    ) -> bool {
        self.allowed.iter().any(|destination| {
            destination.port == port && destination.host.eq_ignore_ascii_case(host)
        })
    }
}

/// A running proxy: the address it listens on and its shutdown handle.
pub struct Proxy {
    address: SocketAddr,
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
        let (shutdown, mut shutdown_rx) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = shutdown_rx.changed() => return,
                    accepted = listener.accept() => {
                        let Ok((stream, _peer)) = accepted else {
                            return;
                        };
                        let egress = egress.clone();
                        let expected = expected.clone();
                        tokio::spawn(async move {
                            if let Err(error) = serve(stream, &egress, &expected).await {
                                tracing::debug!(%error, "a proxy connection ended");
                            }
                        });
                    },
                }
            }
        });
        Ok(Self {
            address,
            token,
            shutdown,
            task,
        })
    }

    /// The loopback address the proxy listens on; its port goes into the sandbox
    /// policy as the proxy grant.
    #[must_use]
    pub const fn address(&self) -> SocketAddr {
        self.address
    }

    /// The proxy URL to put in the child's environment, carrying the credential
    /// so a client sends it as `Proxy-Authorization`. The URL is kept out of
    /// logs; the token is per-proxy and dies with it.
    #[must_use]
    pub fn url(&self) -> String {
        format!("http://agentd:{}@{}", self.token, self.address)
    }

    /// Stops serving.
    pub fn stop(self) {
        let _ = self.shutdown.send(true);
        self.task.abort();
    }
}

/// Serves one client: authenticate, parse `CONNECT`, enforce the allowlist, tunnel.
async fn serve(
    client: TcpStream,
    egress: &Egress,
    expected: &str,
) -> io::Result<()> {
    let (read_half, mut client_write) = client.into_split();
    // The reader is buffered and may hold bytes read past the CONNECT line, so
    // the tunnel copies through it rather than the raw read half.
    let mut client_read = BufReader::new(read_half);

    let mut request = String::new();
    if client_read.read_line(&mut request).await? == 0 {
        return Ok(());
    }
    let Some((host, port)) = parse_connect(&request) else {
        return reply(&mut client_write, 400, "Bad Request").await;
    };
    // Authenticate before revealing anything about the allowlist.
    if read_authorization(&mut client_read).await?.as_deref() != Some(expected) {
        tracing::warn!(%host, %port, "the proxy refused an unauthenticated client");
        return reply(&mut client_write, 407, "Proxy Authentication Required").await;
    }
    if !egress.allows(&host, port) {
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
    client_write
        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .await?;

    let (mut upstream_read, mut upstream_write) = upstream.into_split();
    let to_upstream = tokio::io::copy(&mut client_read, &mut upstream_write);
    let to_client = tokio::io::copy(&mut upstream_read, &mut client_write);
    // Whichever direction ends first tears the tunnel down.
    tokio::select! {
        _ = to_upstream => {},
        _ = to_client => {},
    }
    Ok(())
}

/// Writes an empty HTTP response with `status`.
async fn reply(
    writer: &mut tokio::net::tcp::OwnedWriteHalf,
    status: u16,
    reason: &str,
) -> io::Result<()> {
    writer
        .write_all(format!("HTTP/1.1 {status} {reason}\r\n\r\n").as_bytes())
        .await
}

/// Reads the header lines after the request line and returns the
/// `Proxy-Authorization` value, if any.
async fn read_authorization(
    reader: &mut BufReader<tokio::net::tcp::OwnedReadHalf>
) -> io::Result<Option<String>> {
    let mut authorization = None;
    let mut header = String::new();
    loop {
        header.clear();
        if reader.read_line(&mut header).await? == 0 {
            break;
        }
        let line = header.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("proxy-authorization")
        {
            authorization = Some(value.trim().to_string());
        }
    }
    Ok(authorization)
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
    use super::{Egress, Proxy, basic_authorization, parse_connect};
    use agentd_sandbox::HostPort;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::{TcpListener, TcpStream};

    #[test]
    fn only_allowlisted_destinations_pass() {
        let egress = Egress::new(vec![HostPort {
            host: String::from("api.example.com"),
            port: 443,
        }]);

        assert!(egress.allows("api.example.com", 443));
        assert!(egress.allows("API.EXAMPLE.COM", 443));
        assert!(!egress.allows("api.example.com", 80));
        assert!(!egress.allows("evil.example.com", 443));
        assert!(!Egress::default().allows("api.example.com", 443));
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

    /// As [`connect_through`] but with an explicit `Proxy-Authorization` value
    /// (`None` sends no credential).
    async fn connect_with_authorization(
        proxy: &Proxy,
        host: &str,
        port: u16,
        authorization: &str,
    ) -> std::io::Result<(TcpStream, String)> {
        let mut stream = TcpStream::connect(proxy.address()).await?;
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

        proxy.stop();
    }
}
