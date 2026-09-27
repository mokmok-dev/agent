//! End-to-end tests for the egress `CONNECT` proxy.
//!
//! These need no confinement, so they run on every host, including a CI runner
//! that forbids user namespaces. Each test starts a throwaway TCP upstream, runs
//! a proxy in front of it, and speaks `CONNECT` over the proxy's Unix socket.
#![expect(
    clippy::expect_used,
    reason = "integration test code may panic when a fixture fails"
)]

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use sandbox::egress::{DestinationSet, ProxyConfig, Transport, UnixProxy, bearer};
use sandbox::policy::HostPort;

/// A throwaway TCP server that echoes a fixed banner and returns one line.
struct Upstream {
    port: u16,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Upstream {
    /// Start a server that writes `banner`, then echoes one line back.
    fn start(banner: &'static str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("binds an ephemeral port");
        let port = listener.local_addr().expect("has an address").port();
        let handle = std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let _ = stream.write_all(banner.as_bytes());
                let mut line = String::new();
                let mut reader = BufReader::new(stream.try_clone().expect("clones"));
                if reader.read_line(&mut line).is_ok() {
                    let _ = stream.write_all(line.as_bytes());
                }
                let _ = stream.flush();
            }
        });
        Self {
            port,
            handle: Some(handle),
        }
    }

    fn join(mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// A proxy over a fresh Unix socket with a background accept loop, removed on
/// drop.
struct Proxy {
    inner: Arc<UnixProxy>,
    root: PathBuf,
    stop: Arc<AtomicBool>,
    accept: Option<std::thread::JoinHandle<()>>,
}

impl Proxy {
    fn start(
        tag: &str,
        token: &str,
        destinations: DestinationSet,
    ) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "sandbox-proxy-{tag}-{}-{unique}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let socket = root.join("egress.sock");
        let config = ProxyConfig {
            transport: Transport::UnixSocket(socket.clone()),
            token: token.to_owned(),
            destinations,
        };
        let proxy = Arc::new(UnixProxy::bind(&socket, config).expect("binds the proxy"));

        // Serve each connection on its own thread, so a test never drives the
        // accept loop itself. The loop ends when `stop` is set and a final
        // connection unblocks `accept`.
        let stop = Arc::new(AtomicBool::new(false));
        let serving = Arc::clone(&proxy);
        let stopper = Arc::clone(&stop);
        let accept = std::thread::spawn(move || {
            while !stopper.load(Ordering::Relaxed) {
                if serving.serve_one().is_err() {
                    break;
                }
            }
        });
        Self {
            inner: proxy,
            root,
            stop,
            accept: Some(accept),
        }
    }

    fn path(&self) -> &std::path::Path {
        self.inner.path()
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Unblock `accept` so the loop observes `stop`.
        let _ = UnixStream::connect(self.inner.path());
        if let Some(handle) = self.accept.take() {
            let _ = handle.join();
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Open a connection and send a raw request head, returning the response head.
fn exchange(
    socket: &std::path::Path,
    request: &[u8],
) -> (String, UnixStream) {
    let mut stream = UnixStream::connect(socket).expect("connects to the proxy");
    stream.write_all(request).expect("writes the request");
    stream.flush().expect("flushes");
    let mut reader = BufReader::new(stream.try_clone().expect("clones"));
    let mut status = String::new();
    // Read the blank line that ends the response head.
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).expect("reads") == 0 {
            break;
        }
        let done = line == "\r\n" || line == "\n";
        status.push_str(&line);
        if done {
            break;
        }
    }
    (status, stream)
}

#[test]
fn a_permitted_destination_is_tunnelled_and_the_bytes_flow() {
    let upstream = Upstream::start("HELLO\n");
    let destinations = DestinationSet::of([HostPort::new("127.0.0.1", upstream.port)]);
    let proxy = Proxy::start("allow", "tok", destinations);

    let request = format!(
        "CONNECT 127.0.0.1:{} HTTP/1.1\r\nProxy-Authorization: {}\r\n\r\nping\n",
        upstream.port,
        bearer("tok")
    );
    let (status, mut stream) = exchange(proxy.path(), request.as_bytes());
    assert!(status.starts_with("HTTP/1.1 200"), "status was: {status}");

    // The banner from the upstream arrives through the tunnel, then the echo.
    let mut reader = BufReader::new(stream.try_clone().expect("clones"));
    let mut banner = String::new();
    reader.read_line(&mut banner).expect("reads the banner");
    assert_eq!(banner, "HELLO\n");
    let mut echo = String::new();
    reader.read_line(&mut echo).expect("reads the echo");
    assert_eq!(echo, "ping\n");
    let _ = stream.flush();
    upstream.join();
}

#[test]
fn a_missing_token_is_407() {
    let upstream = Upstream::start("HELLO\n");
    let proxy = Proxy::start(
        "no-token",
        "tok",
        DestinationSet::of([HostPort::new("127.0.0.1", upstream.port)]),
    );
    let request = format!("CONNECT 127.0.0.1:{} HTTP/1.1\r\n\r\n", upstream.port);
    let (status, _) = exchange(proxy.path(), request.as_bytes());
    assert!(status.starts_with("HTTP/1.1 407"), "status was: {status}");
    // The upstream was never contacted, so its accept blocks; leak the thread
    // rather than join, and let the process end it.
    std::mem::forget(upstream);
}

#[test]
fn a_wrong_token_is_407() {
    let upstream = Upstream::start("HELLO\n");
    let proxy = Proxy::start(
        "wrong-token",
        "tok",
        DestinationSet::of([HostPort::new("127.0.0.1", upstream.port)]),
    );
    let request = format!(
        "CONNECT 127.0.0.1:{} HTTP/1.1\r\nProxy-Authorization: {}\r\n\r\n",
        upstream.port,
        bearer("nope")
    );
    let (status, _) = exchange(proxy.path(), request.as_bytes());
    assert!(status.starts_with("HTTP/1.1 407"), "status was: {status}");
    std::mem::forget(upstream);
}

#[test]
fn an_unlisted_destination_is_403() {
    let proxy = Proxy::start(
        "forbidden",
        "tok",
        DestinationSet::of([HostPort::new("allowed.test", 443)]),
    );
    let request = format!(
        "CONNECT denied.test:443 HTTP/1.1\r\nProxy-Authorization: {}\r\n\r\n",
        bearer("tok")
    );
    let (status, _) = exchange(proxy.path(), request.as_bytes());
    assert!(status.starts_with("HTTP/1.1 403"), "status was: {status}");
}

#[test]
fn an_empty_set_forbids_everything() {
    let proxy = Proxy::start("empty", "tok", DestinationSet::empty());
    let request = format!(
        "CONNECT api.example.com:443 HTTP/1.1\r\nProxy-Authorization: {}\r\n\r\n",
        bearer("tok")
    );
    let (status, _) = exchange(proxy.path(), request.as_bytes());
    assert!(status.starts_with("HTTP/1.1 403"), "status was: {status}");
}

#[test]
fn a_malformed_request_is_400() {
    let proxy = Proxy::start("malformed", "tok", DestinationSet::empty());
    let (status, _) = exchange(proxy.path(), b"GET / HTTP/1.1\r\n\r\n");
    assert!(status.starts_with("HTTP/1.1 400"), "status was: {status}");
}

#[test]
fn a_permitted_host_that_does_not_resolve_is_502() {
    let proxy = Proxy::start(
        "unresolvable",
        "tok",
        DestinationSet::of([HostPort::new("no-such-host.invalid", 443)]),
    );
    let request = format!(
        "CONNECT no-such-host.invalid:443 HTTP/1.1\r\nProxy-Authorization: {}\r\n\r\n",
        bearer("tok")
    );
    let (status, _) = exchange(proxy.path(), request.as_bytes());
    assert!(status.starts_with("HTTP/1.1 502"), "status was: {status}");
}

#[test]
fn the_socket_is_removed_when_the_proxy_drops() {
    let path;
    {
        let proxy = Proxy::start("cleanup", "tok", DestinationSet::empty());
        path = proxy.path().to_path_buf();
        assert!(path.exists(), "the socket exists while the proxy is alive");
    }
    assert!(!path.exists(), "the socket is removed on drop");
}
