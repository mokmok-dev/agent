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
