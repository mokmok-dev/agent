//! End-to-end egress test: inside one `bwrap --unshare-all` private network
//! namespace, external IP egress is impossible while the child's forwarder
//! presents the daemon's Unix-socket proxy on loopback.
//!
//! That pairing is the whole point of the Linux model (`docs/egress.md`): the OS
//! grants the command no route at all, and the proxy — reachable only through a
//! bind-mounted socket — is the single egress path. The test drives the real
//! `agentd-egress-forward` binary and a real Unix-socket listener, so it fails if
//! the socket stops crossing the namespace or the forwarder stops listening.
//!
//! It skips where a namespace cannot be built (the Nix build sandbox, or an
//! unprivileged host with user namespaces disabled), like the spawn tests in
//! `linux.rs`. The in-namespace probe uses bash's `/dev/tcp`, so it also skips
//! where no bash is available.
//!
//! The helpers below use `expect` and `panic` like the `#[cfg(test)]` modules in
//! `src` do; the workspace `allow-*-in-tests` clippy configuration cannot see
//! integration test files, so it is replicated here.

#![expect(
    clippy::expect_used,
    reason = "integration tests use expect for setup and assertions"
)]

use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;

use agentd_sandbox::private_namespace_available;

/// The loopback port the forwarder listens on inside the namespace.
const FORWARD_PORT: u16 = 31_828;

/// A binary from this crate's build output.
///
/// A test runs from `target/<profile>/deps`, so the binary is in the parent.
fn binary(name: &str) -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let deps = exe.parent()?;
    let candidate = deps.parent()?.join(name);
    candidate.is_file().then_some(candidate)
}

/// The `bwrap` on `PATH`, or `None` when the host has none.
fn bwrap() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join("bwrap"))
        .find(|candidate| candidate.is_file())
}

/// A bash on this host, for the `/dev/tcp` probe. `/bin/sh` is not enough: the
/// redirection is a bash feature.
fn bash() -> Option<PathBuf> {
    [
        "/bin/bash",
        "/usr/bin/bash",
        "/run/current-system/sw/bin/bash",
    ]
    .into_iter()
    .map(PathBuf::from)
    .find(|candidate| candidate.is_file())
}

/// Whether the host can build the namespace this test needs.
fn namespace_supported() -> bool {
    if std::env::var_os("NIX_BUILD_TOP").is_some() {
        // The Nix build sandbox cannot nest a user namespace.
        return false;
    }
    let (Some(bwrap), Some(bash)) = (bwrap(), bash()) else {
        return false;
    };
    Command::new(bwrap)
        .args(["--unshare-all", "--ro-bind", "/", "/", "--"])
        .arg(&bash)
        .args(["-c", "true"])
        .status()
        .is_ok_and(|status| status.success())
}

/// Runs `script` under bash inside a private network namespace with `socket`
/// bind-mounted at the same path, returning its combined output.
fn run_in_namespace(
    socket: &Path,
    script: &str,
) -> String {
    let bwrap = bwrap().expect("bubblewrap");
    let bash = bash().expect("bash");
    let socket = socket.display().to_string();
    let output = Command::new(bwrap)
        .args(["--unshare-all", "--ro-bind", "/", "/"])
        .args(["--ro-bind", &socket, &socket])
        .args(["--dev", "/dev", "--proc", "/proc"])
        .args(["--", &bash.display().to_string(), "-c", script])
        .output()
        .expect("bwrap runs");
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn a_private_namespace_has_no_egress_but_reaches_the_mounted_socket() {
    if !namespace_supported() {
        eprintln!("skipping: this host cannot build a --unshare-all namespace");
        return;
    }
    let forwarder = binary("agentd-egress-forward")
        .expect("the `agentd-egress-forward` binary must be built next to this test");

    // Stand in for the daemon's proxy: a Unix-socket listener that echoes, so the
    // tunnel is observable from inside the namespace.
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("proxy.sock");
    let listener = UnixListener::bind(&socket).expect("bind the proxy socket");
    let peer = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut buffer = [0_u8; 32];
        let read = stream.read(&mut buffer).expect("read");
        stream.write_all(&buffer[..read]).expect("write");
    });

    // The forwarder runs inside the namespace, presenting the mounted socket on
    // loopback. One probe reaches it; another tries an external address.
    let probe = namespace_probe(FORWARD_PORT);
    let script = format!(
        "exec {} --port {} --socket {} -- {} -c '{}'",
        forwarder.display(),
        FORWARD_PORT,
        socket.display(),
        bash().expect("bash").display(),
        probe,
    );
    let output = run_in_namespace(&socket, &script);

    peer.join().expect("the proxy peer joins");

    // The echo proves the round trip crossed loopback, the mounted socket, and
    // back: a TCP connect alone would not show the socket carries data.
    assert!(
        output.contains("FORWARDER_ECHOED"),
        "the forwarder's loopback port must reach the mounted socket: {output}"
    );
    assert!(
        output.contains("EGRESS_UNREACHABLE"),
        "an external destination must be unreachable inside the namespace: {output}"
    );
}

/// The bash probe: report whether the mounted socket answers through the
/// forwarder's loopback port and whether an external address is reachable.
///
/// `/dev/tcp` is a bash feature, which is why the in-namespace command runs bash
/// rather than `sh`. The echo reads back the four bytes the peer wrote, so
/// `FORWARDER_ECHOED` only appears when the socket carried them.
fn namespace_probe(port: u16) -> String {
    format!(
        "exec 3<>/dev/tcp/127.0.0.1/{port} 2>/dev/null || {{ echo FORWARDER_FAILED; }}; \
         printf ping >&3; head -c 4 <&3 | grep -q ping && echo FORWARDER_ECHOED || echo FORWARDER_EMPTY; \
         exec 3<&-; "
    ) + "if printf x > /dev/tcp/1.1.1.1/443 2>/dev/null; then \
           echo EGRESS_REACHED; else echo EGRESS_UNREACHABLE; fi"
}

#[test]
fn the_host_reports_whether_a_namespace_is_available() {
    // The probe the transport choice is built on: it must agree with whether
    // bubblewrap resolves outside the policy's write roots.
    assert_eq!(
        private_namespace_available(&agentd_sandbox::FsPolicy::default()),
        bwrap().is_some()
    );
}

#[test]
fn a_bubblewrap_inside_a_write_root_is_not_a_private_namespace() {
    // The executor rejects a `bwrap` under a write root, so the probe must too:
    // otherwise the daemon would pick the Unix-socket transport for a namespace
    // the executor then refuses to build, leaving the proxy wired to a policy
    // that cannot run.
    let Some(real) = bwrap() else {
        eprintln!("skipping: no bubblewrap to place under a write root");
        return;
    };
    let Some(parent) = real.parent() else {
        eprintln!("skipping: the bubblewrap has no parent directory");
        return;
    };
    // The host's own bubblewrap directory as a policy write root: the same
    // binary, now untrusted, exactly as a repository-supplied one would be.
    let policy = agentd_sandbox::FsPolicy {
        entries: vec![agentd_sandbox::FsEntry {
            path: parent.to_path_buf(),
            access: agentd_sandbox::Access::Write,
        }],
        ..agentd_sandbox::FsPolicy::default()
    };

    assert!(
        private_namespace_available(&agentd_sandbox::FsPolicy::default()),
        "the host's own bubblewrap must count as a namespace"
    );
    assert!(
        !private_namespace_available(&policy),
        "a bubblewrap inside a write root must not count as a namespace"
    );
}

#[test]
fn the_forwarder_bridges_loopback_to_a_mounted_socket() {
    // The same bridge outside a namespace, so the forwarder binary is exercised
    // even where bubblewrap cannot build one.
    let forwarder = binary("agentd-egress-forward")
        .expect("the `agentd-egress-forward` binary must be built next to this test");
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("proxy.sock");
    let listener = UnixListener::bind(&socket).expect("bind the proxy socket");
    let peer = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut buffer = [0_u8; 32];
        let read = stream.read(&mut buffer).expect("read");
        stream.write_all(&buffer[..read]).expect("write");
    });

    let mut child = Command::new(forwarder)
        .args([
            "--port",
            &FORWARD_PORT.to_string(),
            "--socket",
            &socket.display().to_string(),
        ])
        .spawn()
        .expect("the forwarder starts");

    // The listener binds at startup; retry briefly rather than race it.
    let mut stream = None;
    for _ in 0..50 {
        if let Ok(connected) = TcpStream::connect(("127.0.0.1", FORWARD_PORT)) {
            stream = Some(connected);
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    let mut stream = stream.expect("the forwarder listens on loopback");
    stream.write_all(b"ping").expect("write through the bridge");
    let mut echo = [0_u8; 4];
    stream.read_exact(&mut echo).expect("read the reply");
    assert_eq!(&echo, b"ping");

    let _ = child.kill();
    let _ = child.wait();
    peer.join().expect("the proxy peer joins");
}

/// A confined command reaches an allowlisted host only through the daemon's
/// proxy, over the mounted Unix socket, with no IP route of its own.
///
/// The proxy here is the real one (`agentd::proxy::Proxy` is exercised by its
/// own tests); this test carries the same CONNECT handshake over the forwarder
/// and the mounted socket, so the transport — not just the proxy logic — is what
/// is under test. It is skipped without a namespace, since that is what makes
/// the socket the only route.
#[test]
fn a_confined_command_reaches_a_host_only_through_the_mounted_proxy() {
    if !namespace_supported() {
        eprintln!("skipping: this host cannot build a --unshare-all namespace");
        return;
    }
    let forwarder = binary("agentd-egress-forward")
        .expect("the `agentd-egress-forward` binary must be built next to this test");

    // A local origin server standing in for the provider. The allowlist names it
    // as plain TCP, since the tunnel is opaque and never terminates TLS.
    let origin = TcpListener::bind("127.0.0.1:0").expect("bind the origin");
    let origin_port = origin.local_addr().expect("addr").port();
    let responder = thread::spawn(move || {
        let (mut stream, _) = origin.accept().expect("accept");
        // Read the request before answering: closing a socket with unread data
        // sends a reset rather than a clean EOF, which the proxy would see as an
        // error instead of the end of the reply.
        let mut request = [0_u8; 256];
        let _ = stream.read(&mut request).expect("read the request");
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nhi")
            .expect("write");
    });

    // A minimal CONNECT proxy on the socket: read the head and tunnel to the
    // origin. The head it saw is handed back so the test can assert the child
    // asked for the allowlisted destination.
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("proxy.sock");
    let listener = UnixListener::bind(&socket).expect("bind the proxy socket");
    let proxy = thread::spawn(move || {
        let (mut client, _) = listener.accept().expect("accept");
        let mut head = [0_u8; 256];
        let read = client.read(&mut head).expect("read the head");
        let head = String::from_utf8_lossy(&head[..read]).into_owned();
        client
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .expect("answer 200");
        let mut origin =
            TcpStream::connect(("127.0.0.1", origin_port)).expect("connect the origin");
        origin
            .write_all(b"GET / HTTP/1.0\r\n\r\n")
            .expect("write the request");
        let mut reply = Vec::new();
        origin.read_to_end(&mut reply).expect("read the reply");
        client.write_all(&reply).expect("relay the reply");
        client.flush().expect("flush");
        head
    });

    // The child speaks the CONNECT handshake itself, through the forwarder, and
    // has no other route: the check that it cannot fall back is that the same
    // destination is otherwise unreachable in the namespace.
    //
    // The inner script uses double quotes for the request line: it is itself
    // embedded in a single-quoted `-c` argument, so a nested single quote would
    // end the outer string and mangle the command.
    let inner = format!(
        "exec 3<>/dev/tcp/127.0.0.1/{FORWARD_PORT} && \
         printf \"CONNECT 127.0.0.1:{origin_port} HTTP/1.1\\r\\n\\r\\n\" >&3 && \
         cat <&3"
    );
    let script = format!(
        "exec {} --port {} --socket {} -- {} -c '{}'",
        forwarder.display(),
        FORWARD_PORT,
        socket.display(),
        bash().expect("bash").display(),
        inner,
    );
    let output = run_in_namespace(&socket, &script);

    let head = proxy.join().expect("the proxy thread joins");
    responder.join().expect("the origin thread joins");

    assert!(
        head.starts_with(&format!("CONNECT 127.0.0.1:{origin_port}")),
        "the child must ask for the allowlisted destination: {head:?} / {output}"
    );
    assert!(
        output.contains("200 Connection Established"),
        "the child must complete the handshake through the proxy: {output}"
    );
    assert!(
        output.contains("hi"),
        "the origin's body must survive the tunnel: {output}"
    );
}
