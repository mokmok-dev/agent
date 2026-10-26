#![allow(
    clippy::expect_used,
    reason = "a test reports a broken assumption by panicking"
)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_agentd");
const SOCKET: &str = "bus.sock";
const PATIENCE: Duration = Duration::from_secs(10);

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn scratch(name: &str) -> PathBuf {
    let unique = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("agentd-{name}-{}-{unique}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("creates scratch dir");
    dir
}

fn daemon(dir: &Path) -> Child {
    Command::new(BIN)
        .arg("--state-dir")
        .arg(dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("starts the daemon")
}

fn settled(path: &Path) -> bool {
    let deadline = Instant::now() + PATIENCE;
    while Instant::now() < deadline {
        if path.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

fn once(dir: &Path) -> Output {
    Command::new(BIN)
        .arg("--once")
        .arg("--state-dir")
        .arg(dir)
        .output()
        .expect("runs the daemon")
}

#[test]
fn a_second_daemon_leaves_the_store_to_the_first() {
    let dir = scratch("second");
    let mut first = daemon(&dir);
    assert!(
        settled(&dir.join(SOCKET)),
        "the first daemon binds its socket"
    );

    let second = once(&dir);

    let _ = first.kill();
    let _ = first.wait();

    assert!(
        !second.status.success(),
        "a second daemon opened a store that already had a writer"
    );
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(stderr.contains("another agentd"), "{stderr}");
}

#[test]
fn the_store_is_free_once_the_daemon_that_held_it_is_gone() {
    let dir = scratch("released");
    let mut first = daemon(&dir);
    assert!(settled(&dir.join(SOCKET)));
    let _ = first.kill();
    let _ = first.wait();

    // Killed rather than asked to stop: the kernel releases the lock with the
    // process, so there is nothing to clean up and no stale lock to clear.
    let again = once(&dir);
    assert!(
        again.status.success(),
        "{}",
        String::from_utf8_lossy(&again.stderr)
    );
}
