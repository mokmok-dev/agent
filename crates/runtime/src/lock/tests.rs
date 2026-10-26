#![allow(
    clippy::expect_used,
    reason = "a test reports a broken assumption by panicking"
)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use super::{LOCK_FILE, Lock, LockError};

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn scratch() -> PathBuf {
    let unique = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("agent-lock-{}-{unique}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("creates the directory");
    dir
}

#[test]
fn a_second_taker_is_told_who_holds_the_store() {
    let dir = scratch();
    let held = Lock::take(&dir).expect("the first taker is alone");
    match Lock::take(&dir) {
        Err(LockError::Busy(path)) => assert_eq!(path, dir.join(LOCK_FILE)),
        Err(other) => panic!("unexpected {other:?}"),
        Ok(_) => panic!("two daemons would both think they were alone"),
    }
    drop(held);
}

#[test]
fn the_lock_is_released_when_the_holder_goes_away() {
    let dir = scratch();
    let held = Lock::take(&dir).expect("alone");
    drop(held);
    let again = Lock::take(&dir).expect("a process that ended releases its lock");
    drop(again);
}

#[test]
fn the_lock_file_stays_behind_so_two_processes_never_hold_two_files() {
    let dir = scratch();
    drop(Lock::take(&dir).expect("alone"));
    assert!(
        dir.join(LOCK_FILE).exists(),
        "removing the file would let the next process lock a different one"
    );
}

#[test]
fn a_busy_lock_says_which_file_and_why() {
    let dir = scratch();
    let _held = Lock::take(&dir).expect("alone");
    let message = Lock::take(&dir).expect_err("busy").to_string();
    assert!(message.contains("another agentd"), "{message}");
    assert!(message.contains(LOCK_FILE), "{message}");
}
