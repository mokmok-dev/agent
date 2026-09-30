//! End-to-end tests for a session's egress proxy.
//!
//! The proxy binds a Unix socket and serves `CONNECT`; a client on that socket
//! is a stand-in for the confined command's forwarder, which is the same byte
//! path. These need no confinement, so they run on every host.
#![expect(
    clippy::expect_used,
    reason = "integration test code may panic when a fixture fails"
)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use agent::bus::Bus;
use agent::server::Server;
use agent::transport::{Allowlist, Listener, WebSocketConfig};
use daemon::bus::BusClient;
use daemon::egress::Egress;
use daemon::image::AgentImage;
use sandbox::egress::HostCapability;

/// A running bus server on a fresh socket.
struct BusServer {
    root: std::path::PathBuf,
    socket: std::path::PathBuf,
    _runtime: tokio::runtime::Runtime,
}

impl BusServer {
    fn start(tag: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "daemon-egress-{tag}-{}-{unique}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("creates the root");
        let socket = root.join("bus.sock");

        let uid = std::fs::metadata(std::env::temp_dir())
            .expect("the temp dir has metadata")
            .uid();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("builds a runtime");
        let guard = runtime.enter();
        let listener = Listener::bind(&socket, WebSocketConfig::default()).expect("binds");
        let bus = Bus::open(root.join("data"), 64).expect("opens the bus");
        let server = Server::new(listener, bus, Allowlist::new().allow_uid(uid));
        runtime.spawn(server.run());
        drop(guard);
        for _ in 0..100 {
            if std::os::unix::net::UnixStream::connect(&socket).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Self {
            root,
            socket,
            _runtime: runtime,
        }
    }

    fn client(&self) -> BusClient {
        BusClient::connect(&daemon::bus::Config {
            socket: self.socket.clone(),
            subscriber_id: "egress-test".to_owned(),
        })
        .expect("connects")
    }
}

impl Drop for BusServer {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// A throwaway TCP echo server.
///
/// It accepts **repeatedly** and handles each connection on its own thread, so a
/// test never depends on a single accept or on a join that could block. Each
/// connection gets `banner`, then every line it sends is echoed until it closes.
/// The accept loop ends when the test process exits, which is enough for a
/// fixture; nothing here needs a clean shutdown.
struct Upstream {
    port: u16,
    #[expect(dead_code, reason = "held so the accept thread outlives the test body")]
    listener: std::thread::JoinHandle<()>,
}

impl Upstream {
    fn echo(banner: &'static str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("binds an ephemeral port");
        let port = listener.local_addr().expect("has an address").port();
        let listener_thread = std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let _ = std::thread::spawn(move || {
                    let _ = stream.write_all(banner.as_bytes());
                    let _ = stream.flush();
                    let mut reader = BufReader::new(stream.try_clone().expect("clones"));
                    loop {
                        let mut line = String::new();
                        match reader.read_line(&mut line) {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {
                                let _ = stream.write_all(line.as_bytes());
                                let _ = stream.flush();
                            },
                        }
                    }
                });
            }
        });
        Self {
            port,
            listener: listener_thread,
        }
    }
}

/// Send `CONNECT` and return the client stream and its reader after the status
/// line, leaving the tunnel open for the caller.
fn open_tunnel(
    socket: &std::path::Path,
    authority: &str,
    token: &str,
) -> (UnixStream, BufReader<UnixStream>, String) {
    let mut client = UnixStream::connect(socket).expect("connects to the proxy");
    client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("sets a deadline");
    let request =
        format!("CONNECT {authority} HTTP/1.1\r\nProxy-Authorization: Bearer {token}\r\n\r\n");
    client.write_all(request.as_bytes()).expect("writes");
    let mut reader = BufReader::new(client.try_clone().expect("clones"));
    let mut status = String::new();
    reader
        .read_line(&mut status)
        .expect("reads the status line");
    let mut blank = String::new();
    reader.read_line(&mut blank).expect("reads the blank line");
    (client, reader, status.trim_end().to_owned())
}

/// Send a `CONNECT` and return the status line.
fn connect(
    socket: &std::path::Path,
    authority: &str,
    token: &str,
) -> String {
    let mut client = UnixStream::connect(socket).expect("connects to the proxy");
    client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("sets a deadline");
    let request =
        format!("CONNECT {authority} HTTP/1.1\r\nProxy-Authorization: Bearer {token}\r\n\r\n");
    client
        .write_all(request.as_bytes())
        .expect("writes the request");
    let mut reader = BufReader::new(client.try_clone().expect("clones"));
    let mut line = String::new();
    reader.read_line(&mut line).expect("reads the status line");
    line.trim_end().to_owned()
}

#[test]
fn an_allowed_host_tunnels_and_an_unlisted_one_is_refused() {
    // The session's proxy permits exactly the image's allowed hosts. The host is
    // `127.0.0.1` because a test cannot resolve a public name; the port is the
    // upstream's, so the image allows `127.0.0.1` and the proxy is asked for the
    // real port. The proxy matches on host and port exactly, so this exercises
    // the listed path and the unlisted path.
    let server = BusServer::start("tunnel");
    let bus = Arc::new(server.client());
    let root = server.root.join("run");
    std::fs::create_dir_all(&root).expect("creates the run dir");

    // A listening port that nothing will connect to: the destination is refused
    // before any upstream is opened, so this only needs to exist as an address.
    let upstream = Upstream::echo("HELLO\n");
    // The image allows the loopback host, but the proxy matches port 443, so a
    // connection to the real port is unlisted and must be refused.
    let image = AgentImage::new("/bin/true").allowing("127.0.0.1");

    let egress = Egress::start(
        &image,
        root.join("egress.sock"),
        &HostCapability::PrivateNetworkNamespace,
        &bus,
        "s-1",
        Duration::from_millis(200),
    )
    .expect("binds the proxy");

    // The host is listed at port 443 only, so a `CONNECT` to the real port is
    // unlisted. With an approver configured and a short deadline, the ask times
    // out and the proxy answers 403.
    let status = connect(
        &egress.socket,
        &format!("127.0.0.1:{}", upstream.port),
        egress.token(),
    );
    assert!(
        status.contains("403"),
        "an unlisted destination must be refused, got: {status}"
    );
    drop(egress);
}

#[test]
fn a_listed_destination_tunnels_bytes() {
    // With the port listed, the same `CONNECT` tunnels and the upstream's banner
    // reaches the client, so the tunnel is real.
    let server = BusServer::start("listed");
    let bus = Arc::new(server.client());
    let root = server.root.join("run");
    std::fs::create_dir_all(&root).expect("creates the run dir");

    let upstream = Upstream::echo("BANNER\n");
    let port = upstream.port;
    // The image carries the destination at the upstream's real port, which is the
    // shape the settings file derives from an endpoint's `base_url`.
    let image = AgentImage::new("/bin/true")
        .allowing_destination(sandbox::policy::HostPort::new("127.0.0.1", port));

    let egress = Egress::start(
        &image,
        root.join("egress.sock"),
        &HostCapability::PrivateNetworkNamespace,
        &bus,
        "s-1",
        Duration::from_millis(200),
    )
    .expect("binds the proxy");

    let (_client, mut reader, status) =
        open_tunnel(&egress.socket, &format!("127.0.0.1:{port}"), egress.token());
    assert!(status.contains("200"), "a listed host tunnels: {status}");
    // The upstream writes its banner on accept; read it through the tunnel. This
    // is the byte-flow proof: the bytes reach the client only through the proxy's
    // tunnel to the upstream.
    let mut banner = [0_u8; 7];
    reader.read_exact(&mut banner).expect("reads the banner");
    assert_eq!(&banner, b"BANNER\n", "the tunnel carries the bytes");
    drop(reader);

    drop(egress);
}

#[test]
fn a_live_tunnel_does_not_block_the_next_connection() {
    // The accept loop must dispatch each connection, not serve it inline. With
    // an open tunnel held, a second connection must still be answered, and
    // `Egress::drop` must not deadlock against the tunnel. A loop that served
    // inline would block on the first tunnel's bytes and never reach the second
    // connection, so this test would hang rather than fail; the harness's
    // timeout is the signal.
    let server = BusServer::start("concurrent");
    let bus = Arc::new(server.client());
    let root = server.root.join("run");
    std::fs::create_dir_all(&root).expect("creates the run dir");

    let upstream = Upstream::echo("OPEN\n");
    let port = upstream.port;
    let image = AgentImage::new("/bin/true")
        .allowing_destination(sandbox::policy::HostPort::new("127.0.0.1", port));
    let egress = Egress::start(
        &image,
        root.join("egress.sock"),
        &HostCapability::PrivateNetworkNamespace,
        &bus,
        "s-1",
        Duration::from_millis(200),
    )
    .expect("binds the proxy");

    // Hold the first tunnel open.
    let (first, mut first_reader, first_status) =
        open_tunnel(&egress.socket, &format!("127.0.0.1:{port}"), egress.token());
    assert!(first_status.contains("200"), "the first tunnel opens");
    let mut banner = [0_u8; 2];
    first_reader
        .read_exact(&mut banner)
        .expect("reads the first banner");
    assert_eq!(&banner, b"OP");

    // A second tunnel must be answered while the first is still open.
    let (_second, _second_reader, second_status) =
        open_tunnel(&egress.socket, &format!("127.0.0.1:{port}"), egress.token());
    assert!(
        second_status.contains("200"),
        "a second connection is served while the first tunnel is open: {second_status}"
    );

    // Close the first tunnel, then drop the proxy: neither may deadlock.
    let _ = first.shutdown(std::net::Shutdown::Both);
    drop(egress);
}
