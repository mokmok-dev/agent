use std::sync::atomic::{AtomicU32, Ordering};

use agent_core::event::{Event, EventKind};
use agent_core::ids::{Actor, BranchId, EventId};
use agent_core::time::Timestamp;
use rusqlite::Connection;

use super::{Store, StoreError};

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct Scratch {
    dir: std::path::PathBuf,
    path: std::path::PathBuf,
}

impl Scratch {
    fn new() -> Self {
        let unique = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("agent-store-{}-{unique}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("creates scratch dir");
        let path = dir.join("agent.db");
        Self { dir, path }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn branch(seed: u8) -> BranchId {
    BranchId::from_bytes([seed; 8])
}

fn event(
    branch: BranchId,
    depth: u64,
    prev: Option<EventId>,
    label: &str,
) -> Event {
    let kind = if depth == 0 {
        EventKind::BranchCreated {
            parent: None,
            fork: None,
            label: Some(label.to_string()),
        }
    } else {
        EventKind::BranchLabeled {
            label: label.to_string(),
        }
    };
    Event {
        branch,
        depth,
        prev,
        time: Timestamp::new(1_790_000_000 + i64::try_from(depth).expect("small"), 0)
            .expect("valid"),
        actor: Actor::Process("test".to_string()),
        causation: None,
        correlation: None,
        model: None,
        kind,
    }
}

fn chain(
    store: &mut Store,
    branch: BranchId,
    count: u64,
) {
    let mut prev = None;
    for depth in 0..count {
        let next = event(branch, depth, prev, "main");
        let id = store.append(&next).expect("appends");
        prev = Some(id);
    }
}

#[test]
fn an_open_creates_the_schema_and_a_second_open_reuses_it() {
    let scratch = Scratch::new();
    {
        let store = Store::open(&scratch.path).expect("opens");
        assert_eq!(store.path(), scratch.path);
    }
    let store = Store::open(&scratch.path).expect("reopens");
    let report = store.verify().expect("verifies");
    assert_eq!(report.events, 0);
    assert_eq!(report.branches, 0);
}

#[test]
fn appending_advances_the_chain() {
    let scratch = Scratch::new();
    let mut store = Store::open(&scratch.path).expect("opens");
    chain(&mut store, branch(1), 3);
    let bodies = store.bodies(branch(1)).expect("reads");
    assert_eq!(bodies.len(), 3);
    let report = store.verify().expect("verifies");
    assert_eq!(report.events, 3);
    assert_eq!(report.branches, 1);
}

#[test]
fn a_wrong_predecessor_is_refused() {
    let scratch = Scratch::new();
    let mut store = Store::open(&scratch.path).expect("opens");
    let first = store
        .append(&event(branch(1), 0, None, "a"))
        .expect("appends");
    let wrong = event(branch(1), 1, None, "b");
    let error = store.append(&wrong).expect_err("refuses");
    match error {
        StoreError::PrevMismatch {
            expected, found, ..
        } => {
            assert_eq!(expected, Some(first));
            assert_eq!(found, None);
        },
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn a_first_event_with_a_predecessor_is_refused() {
    let scratch = Scratch::new();
    let mut store = Store::open(&scratch.path).expect("opens");
    let orphan = event(branch(1), 0, Some(EventId::from_bytes([9; 32])), "a");
    let error = store.append(&orphan).expect_err("refuses");
    match error {
        StoreError::PrevMismatch {
            expected, found, ..
        } => {
            assert_eq!(expected, None);
            assert_eq!(found, Some(EventId::from_bytes([9; 32])));
        },
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn a_depth_that_skips_is_refused() {
    let scratch = Scratch::new();
    let mut store = Store::open(&scratch.path).expect("opens");
    let first = store
        .append(&event(branch(1), 0, None, "a"))
        .expect("appends");
    let skipped = event(branch(1), 7, Some(first), "b");
    let error = store.append(&skipped).expect_err("refuses");
    match error {
        StoreError::DepthGap {
            expected, found, ..
        } => {
            assert_eq!(expected, 1);
            assert_eq!(found, 7);
        },
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn two_branches_do_not_share_a_tip() {
    let scratch = Scratch::new();
    let mut store = Store::open(&scratch.path).expect("opens");
    chain(&mut store, branch(1), 2);
    chain(&mut store, branch(2), 1);
    assert_eq!(store.bodies(branch(1)).expect("reads").len(), 2);
    assert_eq!(store.bodies(branch(2)).expect("reads").len(), 1);
    let report = store.verify().expect("verifies");
    assert_eq!(report.events, 3);
    assert_eq!(report.branches, 2);
}

#[test]
fn an_update_is_refused_by_the_database() {
    let scratch = Scratch::new();
    let mut store = Store::open(&scratch.path).expect("opens");
    chain(&mut store, branch(1), 1);
    let connection = Connection::open(&scratch.path).expect("opens raw");
    let error = connection
        .execute("UPDATE events SET body = '{}'", [])
        .expect_err("refuses");
    assert!(error.to_string().contains("append-only"), "{error}");
}

#[test]
fn a_delete_is_refused_by_the_database() {
    let scratch = Scratch::new();
    let mut store = Store::open(&scratch.path).expect("opens");
    chain(&mut store, branch(1), 1);
    let connection = Connection::open(&scratch.path).expect("opens raw");
    let error = connection
        .execute("DELETE FROM events", [])
        .expect_err("refuses");
    assert!(error.to_string().contains("append-only"), "{error}");
}

#[test]
fn verification_catches_a_rewritten_body() {
    let scratch = Scratch::new();
    let mut store = Store::open(&scratch.path).expect("opens");
    chain(&mut store, branch(1), 2);
    let connection = Connection::open(&scratch.path).expect("opens raw");
    connection
        .execute_batch("DROP TRIGGER events_no_update;")
        .expect("drops the guard");
    connection
        .execute(
            "UPDATE events SET body = '{\"tampered\":true}' WHERE depth = 0",
            [],
        )
        .expect("rewrites");
    let error = store.verify().expect_err("detects");
    match error {
        StoreError::Chain { detail, .. } => assert_eq!(detail, "its own hash"),
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn verification_catches_a_removed_middle_event() {
    let scratch = Scratch::new();
    let mut store = Store::open(&scratch.path).expect("opens");
    chain(&mut store, branch(1), 3);
    let connection = Connection::open(&scratch.path).expect("opens raw");
    connection
        .execute_batch("DROP TRIGGER events_no_delete;")
        .expect("drops the guard");
    connection
        .execute("DELETE FROM events WHERE depth = 1", [])
        .expect("removes");
    let error = store.verify().expect_err("detects");
    match error {
        StoreError::Chain { detail, .. } => assert_eq!(detail, "the chain"),
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn verification_catches_a_depth_that_skips() {
    let scratch = Scratch::new();
    let mut store = Store::open(&scratch.path).expect("opens");
    chain(&mut store, branch(1), 3);
    let connection = Connection::open(&scratch.path).expect("opens raw");
    connection
        .execute_batch("DROP TRIGGER events_no_update;")
        .expect("drops the guard");
    let last = store.bodies(branch(1)).expect("reads")[2].0.to_string();
    connection
        .execute(
            "UPDATE events SET depth = 5 WHERE id = ?1",
            rusqlite::params![last],
        )
        .expect("renumbers");
    let error = store.verify().expect_err("detects");
    match error {
        StoreError::Chain { detail, .. } => assert_eq!(detail, "the depth order"),
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn the_bodies_are_the_canonical_bytes() {
    let scratch = Scratch::new();
    let mut store = Store::open(&scratch.path).expect("opens");
    let first = event(branch(1), 0, None, "main");
    let id = store.append(&first).expect("appends");
    let bodies = store.bodies(branch(1)).expect("reads");
    assert_eq!(bodies.len(), 1);
    assert_eq!(bodies[0].0, id);
    assert_eq!(bodies[0].1, agent_core::canonical::canonical(&first));
}

#[test]
fn a_reopen_keeps_every_event() {
    let scratch = Scratch::new();
    let mut store = Store::open(&scratch.path).expect("opens");
    chain(&mut store, branch(1), 2);
    drop(store);
    let store = Store::open(&scratch.path).expect("reopens");
    assert_eq!(store.bodies(branch(1)).expect("reads").len(), 2);
}

#[test]
fn a_missing_directory_is_an_open_error() {
    let error = Store::open(std::path::Path::new("/nonexistent-dir/agent.db")).expect_err("fails");
    match error {
        StoreError::Open { path, .. } => assert!(path.to_string_lossy().contains("nonexistent")),
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn a_corrupt_identifier_is_reported() {
    let scratch = Scratch::new();
    let mut store = Store::open(&scratch.path).expect("opens");
    chain(&mut store, branch(1), 1);
    let connection = Connection::open(&scratch.path).expect("opens raw");
    connection
        .execute_batch("DROP TRIGGER events_no_update;")
        .expect("drops the guard");
    let id = store.bodies(branch(1)).expect("reads")[0].0.to_string();
    connection
        .execute(
            "UPDATE events SET id = 'not-hex' WHERE id = ?1",
            rusqlite::params![id],
        )
        .expect("renames");
    let error = store.verify().expect_err("detects");
    match error {
        StoreError::Identifier(text) => assert_eq!(text, "not-hex"),
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn every_error_explains_itself() {
    let errors = [
        StoreError::Identifier("x".to_string()),
        StoreError::PrevMismatch {
            branch: branch(1),
            expected: None,
            found: None,
        },
        StoreError::DepthGap {
            branch: branch(1),
            expected: 1,
            found: 2,
        },
        StoreError::Chain {
            id: EventId::from_bytes([0; 32]),
            detail: "the chain",
        },
    ];
    for error in errors {
        assert_ne!(error.to_string(), "");
    }
}

#[test]
fn branches_are_listed_in_id_order() {
    let scratch = Scratch::new();
    let mut store = Store::open(&scratch.path).expect("opens");
    for seed in [2_u8, 1] {
        let first = event(branch(seed), 0, None, "main");
        store.append(&first).expect("appends");
    }
    assert_eq!(
        store.branches().expect("lists"),
        vec![branch(1), branch(2)],
        "the order is the one the reactor schedules on"
    );
}

#[test]
fn an_empty_store_has_no_branches() {
    let scratch = Scratch::new();
    let store = Store::open(&scratch.path).expect("opens");
    assert_eq!(store.branches().expect("lists"), Vec::new());
}

#[test]
fn load_returns_typed_events_in_depth_order() {
    let scratch = Scratch::new();
    let mut store = Store::open(&scratch.path).expect("opens");
    let first = event(branch(1), 0, None, "main");
    let second = event(branch(1), 1, Some(first.id()), "next");
    store.append(&first).expect("appends");
    store.append(&second).expect("appends");
    assert_eq!(store.load(branch(1)).expect("loads"), vec![first, second]);
}

#[test]
fn load_on_a_branch_that_is_not_there_is_empty() {
    let scratch = Scratch::new();
    let store = Store::open(&scratch.path).expect("opens");
    assert!(
        store.load(branch(9)).expect("loads").is_empty(),
        "a branch with no rows has no events"
    );
}

#[test]
fn load_refuses_a_body_it_cannot_decode() {
    let scratch = Scratch::new();
    let mut store = Store::open(&scratch.path).expect("opens");
    chain(&mut store, branch(1), 2);
    let connection = Connection::open(&scratch.path).expect("opens raw");
    connection
        .execute_batch("DROP TRIGGER events_no_update;")
        .expect("drops the guard");
    connection
        .execute(
            "UPDATE events SET body = '{\"tampered\":true}' WHERE depth = 0",
            [],
        )
        .expect("rewrites");
    match store.load(branch(1)) {
        Err(StoreError::Decode(_)) => {},
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn a_branch_id_that_is_not_hex_is_refused() {
    let scratch = Scratch::new();
    let mut store = Store::open(&scratch.path).expect("opens");
    chain(&mut store, branch(1), 1);
    let connection = Connection::open(&scratch.path).expect("opens raw");
    connection
        .execute_batch("DROP TRIGGER events_no_update;")
        .expect("drops the guard");
    connection
        .execute("UPDATE events SET branch = 'nope'", [])
        .expect("rewrites");
    match store.branches() {
        Err(StoreError::Identifier(text)) => assert_eq!(text, "nope"),
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn a_read_only_open_does_not_create_the_database() {
    let scratch = Scratch::new();
    let missing = scratch.dir.join("absent.db");
    match Store::open_read_only(&missing) {
        Err(StoreError::Open { path, .. }) => assert_eq!(path, missing),
        other => panic!("unexpected {other:?}"),
    }
    assert!(!missing.exists(), "a read-only open must not create a file");
}

#[test]
fn a_read_only_open_reads_what_the_writer_left() {
    let scratch = Scratch::new();
    let mut writer = Store::open(&scratch.path).expect("opens");
    chain(&mut writer, branch(1), 2);
    let reader = Store::open_read_only(&scratch.path).expect("opens read only");
    assert_eq!(reader.path(), scratch.path);
    let report = reader.verify().expect("verifies");
    assert_eq!(report.events, 2);
    assert_eq!(report.branches, 1);
}

#[test]
fn a_read_only_open_refuses_a_write() {
    let scratch = Scratch::new();
    let mut writer = Store::open(&scratch.path).expect("opens");
    chain(&mut writer, branch(1), 1);
    let mut reader = Store::open_read_only(&scratch.path).expect("opens read only");
    let tip = reader.load(branch(1)).expect("reads")[0].id();
    match reader.append(&event(branch(1), 1, Some(tip), "two")) {
        Err(StoreError::Query(_)) => {},
        other => panic!("unexpected {other:?}"),
    }
}
