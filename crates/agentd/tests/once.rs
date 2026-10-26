#![allow(
    clippy::expect_used,
    reason = "a test reports a broken assumption by panicking"
)]

use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU32, Ordering};

use agent_core::event::{Event, EventKind};
use agent_core::ids::{Actor, BranchId};
use agent_core::time::Timestamp;
use agent_store::Store;

const BIN: &str = env!("CARGO_BIN_EXE_agentd");
const ROOT: BranchId = BranchId::from_bytes([0; 8]);

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn scratch(name: &str) -> PathBuf {
    let unique = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("agentd-{name}-{}-{unique}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("creates scratch dir");
    dir
}

fn run_once(dir: &PathBuf) -> Output {
    Command::new(BIN)
        .arg("--once")
        .arg("--state-dir")
        .arg(dir)
        .output()
        .expect("runs the daemon")
}

#[test]
fn one_shot_starts_the_store_and_reports_nothing_to_do() {
    let dir = scratch("start");
    let output = run_once(&dir);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let store = Store::open(&dir.join("agent.db")).expect("opens");
    let events = store.load(ROOT).expect("loads");
    assert_eq!(events.len(), 2, "{events:?}");
    assert!(matches!(events[0].kind, EventKind::BranchCreated { .. }));
    assert!(matches!(events[1].kind, EventKind::InstanceStarted { .. }));
}

#[test]
fn one_shot_turns_a_command_into_a_turn() {
    let dir = scratch("turn");
    assert!(run_once(&dir).status.success());

    {
        let mut store = Store::open(&dir.join("agent.db")).expect("opens");
        let events = store.load(ROOT).expect("loads");
        let command = Event {
            branch: ROOT,
            depth: u64::try_from(events.len()).expect("small"),
            prev: events.last().map(Event::id),
            time: Timestamp::new(1_790_000_000, 0).expect("valid"),
            actor: Actor::Human("ana".to_string()),
            causation: None,
            correlation: None,
            model: None,
            kind: EventKind::CommandReceived {
                command: "send".to_string(),
                command_id: "c-1".to_string(),
                surface: "test".to_string(),
                text: Some("hello".to_string()),
            },
        };
        store.append(&command).expect("appends");
    }

    assert!(run_once(&dir).status.success());

    let store = Store::open(&dir.join("agent.db")).expect("opens");
    let events = store.load(ROOT).expect("loads");
    let turn = events.last().expect("an event");
    match &turn.kind {
        EventKind::TurnRequested { input, .. } => assert_eq!(input, "hello"),
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(turn.depth, 3);
    assert!(turn.causation.is_some());
}

#[test]
fn a_second_pass_finds_nothing_left_to_do() {
    let dir = scratch("idle");
    assert!(run_once(&dir).status.success());
    assert!(run_once(&dir).status.success());
    let store = Store::open(&dir.join("agent.db")).expect("opens");
    assert_eq!(store.load(ROOT).expect("loads").len(), 2);
}

#[test]
fn an_unknown_flag_fails_and_names_itself() {
    let output = Command::new(BIN)
        .args(["--state-dir", "/tmp", "--verbose"])
        .output()
        .expect("runs the daemon");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unknown argument: --verbose"), "{stderr}");
}

#[test]
fn help_prints_the_usage_line() {
    let output = Command::new(BIN).arg("--help").output().expect("runs");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("usage: agentd"), "{stdout}");
}
