use std::sync::atomic::{AtomicU32, Ordering};

use agent_core::event::{Event, EventKind};
use agent_core::ids::{Actor, BranchId};
use agent_core::time::Timestamp;
use agent_store::Store;

use super::{ROOT_BRANCH, Reactor, Step};

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct Scratch {
    dir: std::path::PathBuf,
    path: std::path::PathBuf,
}

impl Scratch {
    fn new() -> Self {
        let unique = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("agent-runtime-{}-{unique}", std::process::id()));
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

fn now() -> Timestamp {
    Timestamp::new(1_790_000_000, 0).expect("valid")
}

fn send(
    store: &mut Store,
    branch: BranchId,
    text: &str,
) {
    let events = store.load(branch).expect("loads");
    let depth = u64::try_from(events.len()).expect("small");
    let prev = events.last().map(Event::id);
    let event = Event {
        branch,
        depth,
        prev,
        time: now(),
        actor: Actor::Human("ana".to_string()),
        causation: None,
        correlation: None,
        model: None,
        kind: EventKind::CommandReceived {
            command: "send".to_string(),
            command_id: "c-1".to_string(),
            surface: "test".to_string(),
            text: Some(text.to_string()),
        },
    };
    store.append(&event).expect("appends");
}

#[test]
fn a_new_store_gets_a_root_branch_and_a_start_event() {
    let scratch = Scratch::new();
    let mut reactor = Reactor::open(&scratch.path).expect("opens");
    assert_eq!(reactor.start(now()).expect("starts"), Step::Applied);
    let state = reactor.state().expect("folds");
    let root = state.branch(ROOT_BRANCH).expect("root exists");
    assert_eq!(root.depth, 1);
    assert_eq!(root.label.as_deref(), Some("main"));
}

#[test]
fn starting_twice_changes_nothing() {
    let scratch = Scratch::new();
    let mut reactor = Reactor::open(&scratch.path).expect("opens");
    reactor.start(now()).expect("starts");
    assert_eq!(reactor.start(now()).expect("starts"), Step::Idle);
    let events = {
        let store = Store::open(&scratch.path).expect("opens");
        store.load(ROOT_BRANCH).expect("loads").len()
    };
    assert_eq!(events, 2);
}

#[test]
fn a_pending_send_becomes_a_turn() {
    let scratch = Scratch::new();
    let mut reactor = Reactor::open(&scratch.path).expect("opens");
    reactor.start(now()).expect("starts");
    {
        let mut store = Store::open(&scratch.path).expect("opens");
        send(&mut store, ROOT_BRANCH, "hello");
    }
    assert_eq!(reactor.step(now()).expect("steps"), Step::Applied);
    let events = {
        let store = Store::open(&scratch.path).expect("opens");
        store.load(ROOT_BRANCH).expect("loads")
    };
    let turn = events.last().expect("an event");
    match &turn.kind {
        EventKind::TurnRequested { input, .. } => assert_eq!(input, "hello"),
        other => panic!("unexpected {other:?}"),
    }
    assert!(turn.causation.is_some());
    assert!(turn.correlation.is_some());
}

#[test]
fn a_second_step_after_the_turn_is_idle() {
    let scratch = Scratch::new();
    let mut reactor = Reactor::open(&scratch.path).expect("opens");
    reactor.start(now()).expect("starts");
    {
        let mut store = Store::open(&scratch.path).expect("opens");
        send(&mut store, ROOT_BRANCH, "hello");
    }
    reactor.step(now()).expect("steps");
    assert_eq!(reactor.step(now()).expect("steps"), Step::Idle);
}

#[test]
fn the_store_still_verifies_after_the_reactor_writes() {
    let scratch = Scratch::new();
    let mut reactor = Reactor::open(&scratch.path).expect("opens");
    reactor.start(now()).expect("starts");
    {
        let mut store = Store::open(&scratch.path).expect("opens");
        send(&mut store, ROOT_BRANCH, "hello");
    }
    reactor.step(now()).expect("steps");
    let store = Store::open(&scratch.path).expect("opens");
    let report = store.verify().expect("verifies");
    assert_eq!(report.events, 4);
    assert_eq!(report.branches, 1);
}

#[test]
fn the_state_is_the_same_whether_it_is_folded_once_or_twice() {
    let scratch = Scratch::new();
    let mut reactor = Reactor::open(&scratch.path).expect("opens");
    reactor.start(now()).expect("starts");
    {
        let mut store = Store::open(&scratch.path).expect("opens");
        send(&mut store, ROOT_BRANCH, "hello");
    }
    reactor.step(now()).expect("steps");
    let first = reactor.state().expect("folds");
    let second = reactor.state().expect("folds");
    assert_eq!(first, second);
}

#[test]
fn an_event_for_an_unknown_branch_surfaces_as_a_fold_error() {
    let scratch = Scratch::new();
    let mut reactor = Reactor::open(&scratch.path).expect("opens");
    reactor.start(now()).expect("starts");
    {
        let mut store = Store::open(&scratch.path).expect("opens");
        let orphan = Event {
            branch: BranchId::from_bytes([9; 8]),
            depth: 0,
            prev: None,
            time: now(),
            actor: Actor::Process("test".to_string()),
            causation: None,
            correlation: None,
            model: None,
            kind: EventKind::InstanceStopping {
                reason: "test".to_string(),
            },
        };
        store.append(&orphan).expect("appends");
    }
    assert!(reactor.state().is_err());
}

#[test]
fn every_error_explains_itself() {
    use agent_core::fold::FoldError;
    let error = super::ReactorError::Fold(FoldError::UnknownBranch(ROOT_BRANCH));
    assert_ne!(error.to_string(), "");
}
