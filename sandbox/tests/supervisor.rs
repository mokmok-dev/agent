//! End-to-end test for the supervisor running the real `egress-forward`.
//!
//! This needs no confinement, so it runs on every host, including a CI runner
//! that forbids user namespaces. It proves the composition the supervisor exists
//! for: it starts the real forwarder, waits for it to listen, runs a command, and
//! the forwarder bridges a loopback connection to the proxy socket. The namespace
//! and socket-mount halves are covered by `tests/executor.rs`.
#![expect(
    clippy::expect_used,
    clippy::panic,
    reason = "integration test code may panic when a fixture fails"
)]

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use sandbox::supervisor::{SupervisorConfig, run};

/// A throwaway Unix-socket server that echoes lines until the peer closes.
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
            "sandbox-supervisor-{tag}-{}-{unique}",
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
}

impl Drop for UnixEcho {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = std::os::unix::net::UnixStream::connect(&self.path);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// A free loopback port, by binding and dropping.
fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("binds");
    listener.local_addr().expect("has an address").port()
}

/// Read the port the forwarder reports on its stderr.
#[test]
fn the_supervisor_runs_the_real_forwarder_and_the_command_reaches_it() {
    if !std::path::Path::new("/bin/sh").exists() {
        return;
    }
    let echo = UnixEcho::start("e2e");
    let port = free_port();

    // The command is a sleep, so the forwarder stays up while the test connects.
    let config = SupervisorConfig {
        forwarder: PathBuf::from(env!("CARGO_BIN_EXE_egress-forward")),
        socket: echo.path.clone(),
        port,
        command: vec!["/bin/sh".into(), "-c".into(), "sleep 5".into()],
    };

    // The supervisor blocks until the command ends, so run it on a thread and
    // drive the forwarder from the test.
    let serving = std::thread::spawn(move || run(&config));

    // Connect to the forwarder's loopback port; the bytes must reach the socket
    // echo and come back, proving the supervisor started the real forwarder, it
    // is listening on the port, and it bridges to the socket.
    let mut client = connect_with_retry(port);
    client
        .set_read_timeout(Some(Duration::from_secs(3)))
        .expect("sets a read timeout");
    client.write_all(b"hello\n").expect("writes");
    client.flush().expect("flushes");
    let mut reader = BufReader::new(client.try_clone().expect("clones"));
    let mut line = String::new();
    reader.read_line(&mut line).expect("reads the echo");
    assert_eq!(line, "hello\n", "the bytes round-tripped the forwarder");

    drop(client);
    let outcome = serving.join().expect("joins").expect("runs");
    assert_eq!(outcome.code, Some(0), "the command exited cleanly");
}

/// Connect to `port`, retrying until the forwarder is listening.
fn connect_with_retry(port: u16) -> TcpStream {
    for _ in 0..50 {
        if let Ok(stream) = TcpStream::connect(("127.0.0.1", port)) {
            return stream;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("the forwarder never accepted a connection on {port}");
}

#[test]
fn the_supervisor_binary_reports_a_usage_error_on_a_bad_argument() {
    let output = Command::new(env!("CARGO_BIN_EXE_sandbox-supervisor"))
        .args(["--socket", "/tmp/x.sock", "--port", "1", "--", "/bin/true"])
        .output()
        .expect("runs the supervisor");
    assert!(!output.status.success(), "a missing --forward must fail");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--forward"),
        "the usage line should name --forward, got: {stderr}"
    );
}
