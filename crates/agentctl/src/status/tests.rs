//! `status` against a store a daemon would have written.

#![allow(
    clippy::expect_used,
    reason = "a test reports a broken assumption by panicking"
)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use agent_core::event::{Event, EventKind, Risk};
use agent_core::ids::Actor;
use agent_core::ids::{BranchId, CallId, TurnId};
use agent_core::time::Timestamp;
use agent_store::Store;

use super::{last_failure, read};

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn scratch() -> PathBuf {
    let unique = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("agent-status-{}-{unique}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("creates the directory");
    dir.join("agent.db")
}

fn at(seconds: i64) -> Timestamp {
    Timestamp::new(1_790_000_000 + seconds, 0).expect("valid")
}

fn branch() -> BranchId {
    BranchId::from_bytes([1; 8])
}

fn turn() -> TurnId {
    TurnId::from_bytes([2; 8])
}

/// A store with one turn and one question waiting.
fn with_a_question(path: &std::path::Path) {
    let mut store = Store::open(path).expect("opens");
    let mut push = |kind: EventKind, correlation: Option<TurnId>| {
        let events = store.load(branch()).unwrap_or_default();
        let event = Event {
            branch: branch(),
            depth: u64::try_from(events.len()).expect("small"),
            prev: events.last().map(Event::id),
            time: at(i64::try_from(events.len()).unwrap_or(0)),
            actor: Actor::Process("test".to_string()),
            causation: None,
            correlation,
            model: None,
            kind,
        };
        store.append(&event).expect("appends");
    };
    push(
        EventKind::BranchCreated {
            fork: None,
            label: None,
            parent: None,
        },
        None,
    );
    push(
        EventKind::TurnRequested {
            input: "hello".to_string(),
            turn: turn(),
        },
        Some(turn()),
    );
    push(
        EventKind::ToolIntent {
            args: "{}".to_string(),
            call: CallId {
                turn: turn(),
                step: 0,
            },
            key: "k".to_string(),
            tool: "shell".to_string(),
            env: Vec::new(),
        },
        Some(turn()),
    );
    push(
        EventKind::ApprovalRequested {
            call: CallId {
                turn: turn(),
                step: 0,
            },
            expires: at(600),
            risk: Risk::Destructive,
            tool: "shell".to_string(),
        },
        Some(turn()),
    );
}

#[test]
fn a_question_that_is_waiting_is_the_first_thing_an_operator_sees() {
    let path = scratch();
    with_a_question(&path);
    let status = read(&path).expect("reads");
    assert_eq!(status.events, 4);
    assert!(status.bytes > 0, "the store has a size");
    let only = status.branches.first().expect("one branch");
    assert_eq!(only.id, branch());
    assert_eq!(only.depth, 3);
    assert!(!only.archived);
    let waiting = only.waiting.as_ref().expect("the question is waiting");
    assert_eq!(waiting.tool, "shell");
    assert_eq!(waiting.risk, Risk::Destructive);
    assert_eq!(
        waiting.call,
        CallId {
            turn: turn(),
            step: 0
        }
    );
    assert_eq!(waiting.expires, at(600));
}

#[test]
fn a_quiet_store_reports_no_question() {
    let path = scratch();
    {
        let mut store = Store::open(&path).expect("opens");
        store
            .append(&Event {
                branch: branch(),
                depth: 0,
                prev: None,
                time: at(0),
                actor: Actor::Process("test".to_string()),
                causation: None,
                correlation: None,
                model: None,
                kind: EventKind::BranchCreated {
                    fork: None,
                    label: None,
                    parent: None,
                },
            })
            .expect("appends");
    }
    let status = read(&path).expect("reads");
    let only = status.branches.first().expect("one branch");
    assert!(only.waiting.is_none());
    assert_eq!(only.depth, 0);
    assert_eq!(only.tip, Some(at(0)));
}

#[test]
fn the_last_failure_is_reported_with_its_doubt() {
    let events = vec![Event {
        branch: branch(),
        depth: 0,
        prev: None,
        time: at(0),
        actor: Actor::Process("test".to_string()),
        causation: None,
        correlation: None,
        model: None,
        kind: EventKind::ToolFailed {
            call: CallId {
                turn: turn(),
                step: 0,
            },
            class: agent_core::event::ErrorClass::Timeout,
            may_have_happened: true,
        },
    }];
    assert_eq!(
        last_failure(&events).as_deref(),
        Some("tool: timeout, may have happened")
    );
}

#[test]
fn a_model_failure_is_reported_too() {
    let events = vec![Event {
        branch: branch(),
        depth: 0,
        prev: None,
        time: at(0),
        actor: Actor::Process("test".to_string()),
        causation: None,
        correlation: None,
        model: None,
        kind: EventKind::ModelFailed {
            attempt: 1,
            class: agent_core::event::ErrorClass::RateLimited,
            detail: None,
            retryable: true,
            turn: turn(),
        },
    }];
    assert_eq!(
        last_failure(&events).as_deref(),
        Some("model: rate_limited")
    );
}

#[test]
fn a_forked_branch_counts_the_messages_it_inherited() {
    let path = scratch();
    let child = BranchId::from_bytes([2; 8]);
    let mut store = Store::open(&path).expect("opens");
    let created = Event {
        branch: branch(),
        depth: 0,
        prev: None,
        time: at(0),
        actor: Actor::Process("test".to_string()),
        causation: None,
        correlation: None,
        model: None,
        kind: EventKind::BranchCreated {
            fork: None,
            label: None,
            parent: None,
        },
    };
    let created_id = created.id();
    store.append(&created).expect("appends");
    let asked = Event {
        branch: branch(),
        depth: 1,
        prev: Some(created_id),
        time: at(1),
        actor: Actor::Process("test".to_string()),
        causation: None,
        correlation: Some(turn()),
        model: None,
        kind: EventKind::TurnRequested {
            input: "hello".to_string(),
            turn: turn(),
        },
    };
    let asked_id = asked.id();
    store.append(&asked).expect("appends");
    store
        .append(&Event {
            branch: child,
            depth: 2,
            prev: Some(asked_id),
            time: at(2),
            actor: Actor::Process("test".to_string()),
            causation: None,
            correlation: None,
            model: None,
            kind: EventKind::BranchCreated {
                fork: Some(asked_id),
                label: None,
                parent: Some(branch()),
            },
        })
        .expect("appends");
    drop(store);

    let status = read(&path).expect("reads");
    let child_row = status
        .branches
        .iter()
        .find(|row| row.id == child)
        .expect("the child has a row");
    assert_eq!(child_row.depth, 2);
    assert_eq!(
        child_row.messages, 1,
        "the child reads its parent's transcript"
    );
    let parent_row = status
        .branches
        .iter()
        .find(|row| row.id == branch())
        .expect("the parent has a row");
    assert_eq!(
        parent_row.messages, 1,
        "a root still counts only its own messages"
    );
}

#[test]
fn a_model_failure_shows_what_the_failure_said() {
    let events = vec![Event {
        branch: branch(),
        depth: 0,
        prev: None,
        time: at(0),
        actor: Actor::Process("test".to_string()),
        causation: None,
        correlation: None,
        model: None,
        kind: EventKind::ModelFailed {
            attempt: 1,
            class: agent_core::event::ErrorClass::Transport,
            detail: Some("Connection refused (os error 111)".to_string()),
            retryable: true,
            turn: turn(),
        },
    }];
    assert_eq!(
        last_failure(&events).as_deref(),
        Some("model: transport (Connection refused (os error 111))"),
        "the class names the kind, the detail names the occasion"
    );
}

#[test]
fn a_log_that_contradicts_itself_is_refused() {
    let path = scratch();
    let mut store = Store::open(&path).expect("opens");
    let mut push = |kind: EventKind| {
        let events = store.load(branch()).unwrap_or_default();
        let event = Event {
            branch: branch(),
            depth: u64::try_from(events.len()).expect("small"),
            prev: events.last().map(Event::id),
            time: at(i64::try_from(events.len()).unwrap_or(0)),
            actor: Actor::Process("test".to_string()),
            causation: None,
            correlation: None,
            model: None,
            kind,
        };
        store.append(&event).expect("appends");
    };
    push(EventKind::BranchCreated {
        fork: None,
        label: None,
        parent: None,
    });
    for _ in 0..2 {
        push(EventKind::TurnRequested {
            input: "hello".to_string(),
            turn: turn(),
        });
    }
    let error = read(&path).expect_err("refuses the contradiction");
    assert!(error.is_corrupt());
    assert!(error.to_string().contains("contradicts"));
}
