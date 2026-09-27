//! End-to-end tests for the child-side forwarder.
//!
//! These need no confinement: a forwarder is bridged to a throwaway Unix socket
//! server, and a TCP client speaks to the forwarder's loopback port. The test
//! proves the byte bridge and the CLI wiring, which is exactly what runs inside
//! the network namespace. Starting the forwarder alongside the command is the
//! supervisor's job and is covered in a later milestone.
#![expect(
    clippy::expect_used,
    clippy::panic,
    reason = "integration test code may panic when a fixture fails"
)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use sandbox::egress::{ForwardConfig, Forwarder};

/// A throwaway Unix-socket server that echoes each line until the peer closes.
struct UnixEcho {
    path: PathBuf,
    root: PathBuf,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl UnixEcho {
    fn start(tag: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "sandbox-forward-{tag}-{}-{unique}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("creates the root");
        let path = root.join("egress.sock");
        let listener = UnixListener::bind(&path).expect("binds the socket");

        let stop = Arc::new(AtomicBool::new(false));
        let stopper = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            while !stopper.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        // Echo every line until EOF, then the socket closes.
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
                    },
                    Err(_) => break,
                }
            }
        });
        Self {
            path,
            root,
            stop,
            handle: Some(handle),
        }
    }

    fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for UnixEcho {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Unblock `accept` so the loop observes `stop`.
        let _ = UnixStream::connect(&self.path);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Read the port the binary reports on its stderr.
fn read_reported_port(stderr: impl Read) -> u16 {
    let mut reader = BufReader::new(stderr);
    for _ in 0..8 {
        let mut line = String::new();
        if reader.read_line(&mut line).expect("reads stderr") == 0 {
            break;
        }
        if let Some(rest) = line.split("127.0.0.1:").nth(1) {
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            if let Ok(port) = digits.parse() {
                return port;
            }
        }
    }
    panic!("the forwarder never reported its port");
}

/// Spawn the built `egress-forward` binary and return it with its bound port.
fn spawn_binary(socket: &std::path::Path) -> (Child, u16) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_egress-forward"))
        .args(["--socket"])
        .arg(socket)
        .args(["--port", "0"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawns the forwarder");
    let port = read_reported_port(child.stderr.take().expect("has a stderr"));
    (child, port)
}

#[test]
fn the_forwarder_bridges_loopback_to_the_socket() {
    let echo = UnixEcho::start("library");
    let config = ForwardConfig {
        port: 0,
        socket: echo.path().to_path_buf(),
    };
    let forwarder = Forwarder::bind(&config).expect("binds the forwarder");
    let port = forwarder.port();

    // Serve on a background thread; it ends when the test ends the process.
    std::thread::spawn(move || forwarder.run());

    let mut client = TcpStream::connect(("127.0.0.1", port)).expect("connects");
    client
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("sets a read timeout");
    client.write_all(b"hello\n").expect("writes");
    client.flush().expect("flushes");

    let mut reader = BufReader::new(client.try_clone().expect("clones"));
    let mut echo_line = String::new();
    reader.read_line(&mut echo_line).expect("reads the echo");
    assert_eq!(echo_line, "hello\n", "the bytes round-tripped the socket");
}

#[test]
fn a_client_half_close_still_receives_the_reply() {
    let echo = UnixEcho::start("half-close");
    let config = ForwardConfig {
        port: 0,
        socket: echo.path().to_path_buf(),
    };
    let forwarder = Forwarder::bind(&config).expect("binds the forwarder");
    let port = forwarder.port();
    std::thread::spawn(move || forwarder.run());

    let mut client = TcpStream::connect(("127.0.0.1", port)).expect("connects");
    client
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("sets a read timeout");
    client.write_all(b"ping\n").expect("writes");
    client.flush().expect("flushes");
    // Shut the write half; the echo should still arrive.
    client
        .shutdown(std::net::Shutdown::Write)
        .expect("half-closes");

    let mut reader = BufReader::new(client.try_clone().expect("clones"));
    let mut reply = String::new();
    reader.read_line(&mut reply).expect("reads the reply");
    assert_eq!(reply, "ping\n", "a half-close still receives the reply");
}

#[test]
fn the_binary_reports_its_port_and_bridges_bytes() {
    let echo = UnixEcho::start("binary");
    let (mut child, port) = spawn_binary(echo.path());

    let mut client = TcpStream::connect(("127.0.0.1", port)).expect("connects");
    client
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("sets a read timeout");
    client.write_all(b"via-binary\n").expect("writes");
    client.flush().expect("flushes");
    let mut reader = BufReader::new(client.try_clone().expect("clones"));
    let mut reply = String::new();
    reader.read_line(&mut reply).expect("reads the reply");
    assert_eq!(reply, "via-binary\n");

    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn the_binary_rejects_a_missing_argument() {
    let output = Command::new(env!("CARGO_BIN_EXE_egress-forward"))
        .arg("--socket")
        .arg("/tmp/x.sock")
        .output()
        .expect("runs the forwarder");
    assert!(!output.status.success(), "a missing --port must fail");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--port"),
        "the usage line should name --port, got: {stderr}"
    );
}
