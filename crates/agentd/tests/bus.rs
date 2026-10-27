//! The socket against the real daemon process.
//!
//! The unit tests drive the loop directly. This one proves the pieces a client
//! actually meets: the daemon answers, a refusal does not end the process, and a
//! fork's reply names a branch the store already holds.

#![allow(
    clippy::expect_used,
    reason = "a test reports a broken assumption by panicking"
)]

use std::io::{BufRead as _, BufReader, Write as _};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use agent_store::Store;

const BIN: &str = env!("CARGO_BIN_EXE_agentd");

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct Scratch {
    dir: PathBuf,
    child: Child,
}

impl Scratch {
    fn start() -> Self {
        let unique = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("agentd-bus-{}-{unique}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("creates the state directory");
        let child = Command::new(BIN)
            .arg("--state-dir")
            .arg(&dir)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawns the daemon");
        let scratch = Self { dir, child };
        scratch.wait_for_socket();
        scratch
    }

    fn socket(&self) -> PathBuf {
        self.dir.join("bus.sock")
    }

    fn wait_for_socket(&self) {
        let started = Instant::now();
        while !self.socket().exists() {
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "the daemon never bound {}",
                self.socket().display()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn ask(
        &self,
        line: &str,
    ) -> String {
        let mut stream = std::os::unix::net::UnixStream::connect(self.socket()).expect("connects");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("sets a timeout");
        stream.write_all(line.as_bytes()).expect("writes");
        stream.write_all(b"\n").expect("writes");
        stream.flush().expect("flushes");
        let mut answer = String::new();
        BufReader::new(stream)
            .read_line(&mut answer)
            .expect("reads");
        answer
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn string_field<'a>(
    line: &'a str,
    key: &str,
) -> Option<&'a str> {
    let marker = format!("\"{key}\":\"");
    let start = line.find(&marker)? + marker.len();
    let rest = &line[start..];
    Some(&rest[..rest.find('"')?])
}

/// The daemon answers a bad line, survives it, and still forks.
#[test]
fn the_daemon_refuses_a_bad_line_and_keeps_serving() {
    let scratch = Scratch::start();

    let malformed = scratch.ask("not json");
    assert!(malformed.contains(r#""ok":false"#), "{malformed}");
    assert!(malformed.contains(r#""class":"malformed""#), "{malformed}");

    let missing = scratch.ask(r#"{"command":"send","branch":"b-ffffffffffffffff","text":"hello"}"#);
    assert!(missing.contains(r#""class":"usage""#), "{missing}");
    assert!(
        missing.contains("no branch b-ffffffffffffffff"),
        "{missing}"
    );

    let forked = scratch.ask(r#"{"command":"fork","parent":"b-0000000000000000"}"#);
    assert!(forked.contains(r#""ok":true"#), "{forked}");
    let branch = string_field(&forked, "branch").expect("a branch");
    let parsed: agent_core::ids::BranchId = branch.parse().expect("a branch id");
    assert!(forked.contains(r#""depth":2"#), "{forked}");

    let store = Store::open(&scratch.dir.join("agent.db")).expect("opens");
    let events = store.load(parsed).expect("the replied branch is stored");
    assert_eq!(events.len(), 1, "{events:?}");
    assert!(matches!(
        events[0].kind,
        agent_core::event::EventKind::BranchCreated { .. }
    ));
}
