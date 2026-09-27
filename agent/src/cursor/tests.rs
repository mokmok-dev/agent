//! Tests for the durable cursor store: persistence, atomic replacement, the
//! resume rule, and rejection of unsafe subscriber IDs.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use super::*;

/// A unique, empty cursor directory removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agent-cursor-{tag}-{}-{unique}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn an_unknown_subscriber_has_no_cursor() {
    let dir = TempDir::new("unknown");
    let store = CursorStore::open(&dir.0).expect("opens");
    let key = CursorKey::new(1000, "audit-log");
    assert_eq!(store.load(&key).expect("loads"), None);
}

#[test]
fn a_stored_cursor_survives_a_reopen() {
    let dir = TempDir::new("persist");
    let key = CursorKey::new(1000, "audit-log");

    {
        let store = CursorStore::open(&dir.0).expect("opens");
        store.store(&key, 1024).expect("stores");
    }

    let reopened = CursorStore::open(&dir.0).expect("reopens");
    assert_eq!(reopened.load(&key).expect("loads"), Some(1024));
}

#[test]
fn storing_a_cursor_twice_keeps_the_latest() {
    let dir = TempDir::new("overwrite");
    let store = CursorStore::open(&dir.0).expect("opens");
    let key = CursorKey::new(1000, "s");

    store.store(&key, 1).expect("stores");
    store.store(&key, 99).expect("stores again");
    assert_eq!(store.load(&key).expect("loads"), Some(99));
}

#[test]
fn cursors_are_isolated_by_uid() {
    let dir = TempDir::new("uid");
    let store = CursorStore::open(&dir.0).expect("opens");

    store.store(&CursorKey::new(1000, "s"), 1).expect("stores");
    store.store(&CursorKey::new(2000, "s"), 2).expect("stores");

    assert_eq!(
        store.load(&CursorKey::new(1000, "s")).expect("loads"),
        Some(1)
    );
    assert_eq!(
        store.load(&CursorKey::new(2000, "s")).expect("loads"),
        Some(2)
    );
}

#[test]
fn cursors_are_isolated_by_subscriber_id() {
    let dir = TempDir::new("subscriber");
    let store = CursorStore::open(&dir.0).expect("opens");

    store.store(&CursorKey::new(1, "a"), 10).expect("stores");
    store.store(&CursorKey::new(1, "b"), 20).expect("stores");

    assert_eq!(
        store.load(&CursorKey::new(1, "a")).expect("loads"),
        Some(10)
    );
    assert_eq!(
        store.load(&CursorKey::new(1, "b")).expect("loads"),
        Some(20)
    );
}

#[test]
fn resolve_takes_the_greater_of_requested_and_stored() {
    let dir = TempDir::new("resolve");
    let store = CursorStore::open(&dir.0).expect("opens");
    let key = CursorKey::new(1, "s");

    // No stored cursor: the requested position wins.
    assert_eq!(store.resolve(&key, 50).expect("resolves"), 50);

    store.store(&key, 100).expect("stores");
    // Stored is ahead: resuming never skips acknowledged events.
    assert_eq!(store.resolve(&key, 50).expect("resolves"), 100);
    // Requested is ahead: the client may ask for a later point.
    assert_eq!(store.resolve(&key, 200).expect("resolves"), 200);
}

#[test]
fn an_unsafe_subscriber_id_is_rejected() {
    let dir = TempDir::new("unsafe");
    let store = CursorStore::open(&dir.0).expect("opens");

    for bad in ["", "..", ".", "a/b", "a\\b", "a b", "süß"] {
        let key = CursorKey::new(1, bad);
        assert!(
            matches!(store.load(&key), Err(CursorError::InvalidSubscriberId(_))),
            "`{bad}` must be rejected",
        );
    }
}

#[test]
fn an_overlong_subscriber_id_is_rejected() {
    let dir = TempDir::new("overlong");
    let store = CursorStore::open(&dir.0).expect("opens");
    let key = CursorKey::new(1, "a".repeat(129));
    assert!(matches!(
        store.store(&key, 1),
        Err(CursorError::InvalidSubscriberId(_))
    ));
}

#[test]
fn a_subscriber_id_at_the_length_limit_is_accepted() {
    let dir = TempDir::new("at-limit");
    let store = CursorStore::open(&dir.0).expect("opens");
    // The limit is inclusive: 128 characters is accepted, 129 is not.
    let key = CursorKey::new(1, "a".repeat(128));
    store.store(&key, 1).expect("the limit length is allowed");
    assert_eq!(store.load(&key).expect("loads"), Some(1));
}

#[test]
fn a_corrupt_cursor_file_is_reported() {
    let dir = TempDir::new("corrupt");
    let store = CursorStore::open(&dir.0).expect("opens");
    let key = CursorKey::new(1, "s");

    // Write the file the store would use, with unparseable contents.
    let path = store.path_for(&key).expect("valid key");
    fs::create_dir_all(path.parent().expect("has parent")).expect("creates dirs");
    fs::write(&path, b"not a number").expect("writes");

    assert!(matches!(store.load(&key), Err(CursorError::Corrupt { .. })));
}

#[test]
fn a_stored_cursor_leaves_no_temporary_file() {
    let dir = TempDir::new("no-temp");
    let store = CursorStore::open(&dir.0).expect("opens");
    store.store(&CursorKey::new(1, "s"), 7).expect("stores");

    // Only the cursor file remains; the temporary was renamed away.
    let uid_dir = dir.0.join(format!("{:08x}", 1));
    let entries: Vec<_> = fs::read_dir(&uid_dir)
        .expect("reads")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(entries, ["s.cursor"]);
}
