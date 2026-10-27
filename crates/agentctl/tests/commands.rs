#![allow(
    clippy::expect_used,
    reason = "a test reports a broken assumption by panicking"
)]

use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU32, Ordering};

use agent_store::Store;

const BIN: &str = env!("CARGO_BIN_EXE_agentctl");
const BRANCH: &str = "b-0123456789abcdef";
const EVENT: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new() -> Self {
        let unique = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("agentctl-{}-{unique}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("creates scratch dir");
        Self { dir }
    }

    fn store(&self) -> PathBuf {
        self.dir.join("agent.db")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn run(args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .output()
        .expect("the binary runs")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn version_prints_one_line_and_exits_zero() {
    let output = run(&["version"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        stdout(&output).trim_end(),
        format!("agentctl {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn version_in_json_mode_carries_its_schema() {
    let output = run(&["version", "--json"]);
    assert_eq!(output.status.code(), Some(0));
    let parsed: serde_json::Value =
        serde_json::from_str(stdout(&output).trim_end()).expect("one json object");
    assert_eq!(parsed["schema"], "agentctl.version/1");
    assert_eq!(parsed["version"], env!("CARGO_PKG_VERSION"));
}

#[test]
fn verify_reports_a_store_it_can_read() {
    let scratch = Scratch::new();
    drop(Store::open(&scratch.store()).expect("creates the store"));
    let output = run(&["verify", "--state-dir", &scratch.dir.to_string_lossy()]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(stdout(&output).trim_end(), "ok: 0 events across 0 branches");
}

#[test]
fn verify_in_json_mode_reports_the_same_counts() {
    let scratch = Scratch::new();
    drop(Store::open(&scratch.store()).expect("creates the store"));
    let output = run(&[
        "verify",
        "--state-dir",
        &scratch.dir.to_string_lossy(),
        "--json",
    ]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let parsed: serde_json::Value =
        serde_json::from_str(stdout(&output).trim_end()).expect("one json object");
    assert_eq!(parsed["schema"], "agentctl.verify/1");
    assert_eq!(parsed["ok"], true);
    assert_eq!(parsed["events"], 0);
    assert_eq!(parsed["branches"], 0);
}

#[test]
fn verify_reads_the_state_directory_from_the_environment() {
    let scratch = Scratch::new();
    drop(Store::open(&scratch.store()).expect("creates the store"));
    let output = Command::new(BIN)
        .args(["verify"])
        .env("STATE_DIRECTORY", &scratch.dir)
        .output()
        .expect("the binary runs");
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
}

#[test]
fn verify_does_not_create_a_store_that_is_not_there() {
    let scratch = Scratch::new();
    let output = run(&["verify", "--state-dir", &scratch.dir.to_string_lossy()]);
    assert_eq!(output.status.code(), Some(70), "{}", stderr(&output));
    assert!(
        !scratch.store().exists(),
        "a read-only command wrote a file"
    );
    assert!(stderr(&output).contains("error:"));
}

#[test]
fn verify_reports_a_corrupt_store_with_its_own_code() {
    let scratch = Scratch::new();
    drop(Store::open(&scratch.store()).expect("creates the store"));
    let connection = rusqlite::Connection::open(scratch.store()).expect("opens raw");
    connection
        .execute_batch("DROP TRIGGER events_no_update;")
        .expect("drops the guard");
    connection
        .execute(
            "INSERT INTO events (id, branch, depth, prev, type, time, actor, body)
             VALUES ('nope', 'b-0000000000000001', 0, NULL, 'branch.created', '2026-10-26T00:00:00Z', 'human:a', '{}')",
            [],
        )
        .expect("writes a row that cannot be its own hash");
    let output = run(&[
        "verify",
        "--state-dir",
        &scratch.dir.to_string_lossy(),
        "--json",
    ]);
    assert_eq!(output.status.code(), Some(74), "{}", stderr(&output));
    let parsed: serde_json::Value =
        serde_json::from_str(stderr(&output).trim_end()).expect("one json object");
    assert_eq!(parsed["error"]["code"], 74);
    assert_eq!(parsed["error"]["class"], "corrupt");
    assert!(parsed["error"]["hint"].is_string());
}

#[test]
fn a_command_with_nothing_behind_it_exits_64_and_says_what_it_needs() {
    let output = run(&["replay", "--branch", BRANCH]);
    assert_eq!(output.status.code(), Some(64));
    assert_eq!(stdout(&output), "");
    let message = stderr(&output);
    assert!(message.contains("not available in this build"), "{message}");
    assert!(message.contains("`agentctl replay`"), "{message}");

    let object = run(&["replay", "--branch", BRANCH, "--json"]);
    assert_eq!(object.status.code(), Some(64));
    let parsed: serde_json::Value =
        serde_json::from_str(stderr(&object).trim_end()).expect("one json object");
    assert_eq!(parsed["error"]["code"], 64);
    assert_eq!(parsed["error"]["class"], "unsupported");
}

#[test]
fn status_reports_what_the_log_says() {
    let scratch = Scratch::new();
    seed_one_turn(&scratch);

    let output = ctl(&scratch.dir, &["status"]);
    assert!(output.status.success(), "{output:?}");
    let text = stdout(&output);
    assert!(text.contains("store  2 events"), "{text}");
    assert!(text.contains(BRANCH), "{text}");
    assert!(text.contains("depth 1"), "{text}");

    let object = ctl(&scratch.dir, &["status", "--json"]);
    assert!(object.status.success(), "{object:?}");
    let parsed: serde_json::Value =
        serde_json::from_str(stdout(&object).trim_end()).expect("one json object");
    assert_eq!(parsed["schema"], "agentctl.status/1");
    assert_eq!(parsed["events"], 2);
    assert_eq!(parsed["branches"][0]["id"], BRANCH);

    let empty = Scratch::new();
    let missing = ctl(&empty.dir, &["status"]);
    assert_eq!(missing.status.code(), Some(78), "{missing:?}");
    assert!(
        stderr(&missing).contains("has the daemon ever run?"),
        "{missing:?}"
    );
}

/// A store with a branch and one turn, which is what a daemon leaves behind.
fn seed_one_turn(scratch: &Scratch) {
    use agent_core::event::{Event, EventKind};
    use agent_core::ids::{Actor, BranchId, TurnId};
    use agent_core::time::Timestamp;

    let branch: BranchId = BRANCH.parse().expect("a branch id");
    let turn = TurnId::from_bytes([2; 8]);
    let mut store = Store::open(&scratch.store()).expect("opens");
    let mut push = |kind: EventKind, correlation| {
        let events = store.load(branch).unwrap_or_default();
        let event = Event {
            branch,
            depth: u64::try_from(events.len()).expect("small"),
            prev: events.last().map(Event::id),
            time: Timestamp::new(1_790_000_000, 0).expect("valid"),
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
            turn,
        },
        Some(turn),
    );
}

#[test]
fn a_message_the_operator_quoted_is_not_read_as_a_flag() {
    let scratch = Scratch::new();
    let socket = scratch.dir.join("bus.sock");
    let daemon = fake_daemon(&socket, r#"{"ok":true}"#);

    // Everything after `--` is positional, so the global option goes before the
    // command.
    let output = Command::new(BIN)
        .arg("--state-dir")
        .arg(&scratch.dir)
        .args(["send", "--branch", BRANCH, "--", "--wait"])
        .output()
        .expect("runs");
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        daemon.join().expect("the daemon finished"),
        r#"{"branch":"b-0123456789abcdef","command":"send","text":"--wait"}"#,
        "the message parsed as a message, not as a flag"
    );
}

#[test]
fn a_fork_that_names_two_points_is_refused_before_anything_runs() {
    let output = run(&[
        "fork",
        "--branch",
        BRANCH,
        "--at",
        "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff",
        "--rewind",
        "2",
    ]);
    assert_eq!(output.status.code(), Some(2));
    assert_ne!(stderr(&output), "");
}

#[test]
fn fork_defaults_to_the_tip_and_prints_the_new_branch() {
    let scratch = Scratch::new();
    let socket = scratch.dir.join("bus.sock");
    let daemon = fake_daemon(
        &socket,
        r#"{"ok":true,"branch":"b-fedcba9876543210","depth":3}"#,
    );

    let output = ctl(&scratch.dir, &["fork", "--branch", BRANCH]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(
        stdout(&output).trim_end(),
        "forked b-fedcba9876543210 at depth 3"
    );

    let line = daemon.join().expect("the daemon finished");
    assert_eq!(
        line, r#"{"command":"fork","parent":"b-0123456789abcdef"}"#,
        "no selector means the tip, so no selector key is sent"
    );
}

#[test]
fn fork_in_json_mode_reports_the_new_branch_and_its_depth() {
    let scratch = Scratch::new();
    let socket = scratch.dir.join("bus.sock");
    let daemon = fake_daemon(
        &socket,
        r#"{"ok":true,"branch":"b-fedcba9876543210","depth":3}"#,
    );

    let output = ctl(&scratch.dir, &["fork", "--branch", BRANCH, "--json"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let parsed: serde_json::Value =
        serde_json::from_str(stdout(&output).trim_end()).expect("one json object");
    assert_eq!(parsed["schema"], "agentctl.fork/1");
    assert_eq!(parsed["branch"], "b-fedcba9876543210");
    assert_eq!(parsed["depth"], 3);

    let line = daemon.join().expect("the daemon finished");
    assert_eq!(line, r#"{"command":"fork","parent":"b-0123456789abcdef"}"#);
}

#[test]
fn fork_sends_the_point_and_the_flags_it_was_given() {
    let scratch = Scratch::new();
    let socket = scratch.dir.join("bus.sock");
    let daemon = fake_daemon(
        &socket,
        r#"{"ok":true,"branch":"b-fedcba9876543210","depth":4}"#,
    );

    let output = ctl(
        &scratch.dir,
        &[
            "fork",
            "--branch",
            BRANCH,
            "--at",
            EVENT,
            "--label",
            "experiment",
            "--boundary",
            "--force",
        ],
    );
    assert_eq!(output.status.code(), Some(0), "{output:?}");

    let line = daemon.join().expect("the daemon finished");
    assert_eq!(
        line,
        format!(
            "{{\"at\":\"{EVENT}\",\"boundary\":true,\"command\":\"fork\",\"force\":true,\
             \"label\":\"experiment\",\"parent\":\"{BRANCH}\"}}"
        )
    );
}

#[test]
fn a_fork_refused_by_policy_exits_65_with_the_message() {
    let scratch = Scratch::new();
    let socket = scratch.dir.join("bus.sock");
    let daemon = fake_daemon(
        &socket,
        r#"{"ok":false,"class":"policy","message":"the fork point leaves a call unresolved; pass --force"}"#,
    );

    let output = ctl(&scratch.dir, &["fork", "--branch", BRANCH]);
    assert_eq!(output.status.code(), Some(65), "{output:?}");
    assert_eq!(stdout(&output), "");
    assert!(
        stderr(&output).contains("the fork point leaves a call unresolved"),
        "{output:?}"
    );
    let _ = daemon.join();
}

#[test]
fn a_corrupt_fork_reply_exits_74() {
    let scratch = Scratch::new();
    let socket = scratch.dir.join("bus.sock");
    let daemon = fake_daemon(
        &socket,
        r#"{"ok":false,"class":"corrupt","message":"the store disagrees with the fold"}"#,
    );

    let output = ctl(&scratch.dir, &["fork", "--branch", BRANCH]);
    assert_eq!(output.status.code(), Some(74), "{output:?}");
    assert!(
        stderr(&output).contains("the store disagrees with the fold"),
        "{output:?}"
    );
    assert_eq!(stderr(&output).matches("error:").count(), 1, "{output:?}");
    let _ = daemon.join();
}

#[test]
fn a_fork_without_a_daemon_still_exits_78() {
    let scratch = Scratch::new();
    let output = ctl(&scratch.dir, &["fork", "--branch", BRANCH]);
    assert_eq!(output.status.code(), Some(78), "{output:?}");
    assert!(
        stderr(&output).contains("no daemon is listening"),
        "{output:?}"
    );
}

#[test]
fn help_is_text_on_stdout_and_exits_zero() {
    let output = run(&["--help"]);
    assert_eq!(output.status.code(), Some(0));
    let text = stdout(&output);
    assert!(text.contains("agentctl"), "{text}");
    assert!(text.contains("verify"), "{text}");
    assert_eq!(stderr(&output), "");
}

/// A socket that takes one line, answers it, and reports what it saw.
fn fake_daemon(
    path: &std::path::Path,
    answer: &str,
) -> std::thread::JoinHandle<String> {
    let listener = std::os::unix::net::UnixListener::bind(path).expect("binds");
    let answer = answer.to_string();
    std::thread::spawn(move || {
        use std::io::{BufRead as _, BufReader, Write as _};
        let (mut stream, _) = listener.accept().expect("accepts");
        let mut reader = BufReader::new(stream.try_clone().expect("clones"));
        let mut line = String::new();
        reader.read_line(&mut line).expect("reads");
        stream.write_all(answer.as_bytes()).expect("writes");
        stream.write_all(b"\n").expect("writes");
        stream.flush().expect("flushes");
        line.trim_end().to_string()
    })
}

fn ctl(
    dir: &std::path::Path,
    arguments: &[&str],
) -> Output {
    Command::new(BIN)
        .args(arguments)
        .arg("--state-dir")
        .arg(dir)
        .output()
        .expect("runs")
}

#[test]
fn send_writes_one_line_the_daemon_can_read() {
    let scratch = Scratch::new();
    let socket = scratch.dir.join("bus.sock");
    let daemon = fake_daemon(&socket, r#"{"ok":true}"#);

    let output = ctl(&scratch.dir, &["send", "--branch", BRANCH, "hello there"]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "send: taken"
    );

    let line = daemon.join().expect("the daemon finished");
    assert_eq!(
        line, r#"{"branch":"b-0123456789abcdef","command":"send","text":"hello there"}"#,
        "the line is the one the socket documents"
    );
}

#[test]
fn approve_carries_the_call_it_answers() {
    let scratch = Scratch::new();
    let socket = scratch.dir.join("bus.sock");
    let daemon = fake_daemon(&socket, r#"{"ok":true}"#);

    let output = ctl(&scratch.dir, &["approve", "0102030405060708:0"]);
    assert!(output.status.success(), "{output:?}");

    let line = daemon.join().expect("the daemon finished");
    assert_eq!(line, r#"{"command":"approve","id":"0102030405060708:0"}"#);
}

#[test]
fn an_approval_on_a_fork_names_its_branch() {
    let scratch = Scratch::new();
    let socket = scratch.dir.join("bus.sock");
    let daemon = fake_daemon(&socket, r#"{"ok":true}"#);

    let output = ctl(
        &scratch.dir,
        &["approve", "--branch", BRANCH, "0102030405060708:0"],
    );
    assert!(output.status.success(), "{output:?}");

    let line = daemon.join().expect("the daemon finished");
    assert_eq!(
        line,
        format!(r#"{{"branch":"{BRANCH}","command":"approve","id":"0102030405060708:0"}}"#)
    );
}

#[test]
fn a_daemon_that_says_no_is_a_policy_failure() {
    let scratch = Scratch::new();
    let socket = scratch.dir.join("bus.sock");
    let daemon = fake_daemon(
        &socket,
        r#"{"ok":false,"class":"policy","message":"deny needs text"}"#,
    );

    let output = ctl(&scratch.dir, &["deny", "0102030405060708:0"]);
    assert_eq!(output.status.code(), Some(65), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("deny needs text"),
        "{output:?}"
    );
    let _ = daemon.join();
}

#[test]
fn no_daemon_is_a_configuration_failure_where_the_operator_typed_it() {
    let scratch = Scratch::new();
    // Nothing is listening: the socket file is not there at all.
    let output = ctl(&scratch.dir, &["send", "--branch", BRANCH, "hello"]);
    assert_eq!(output.status.code(), Some(78), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("no daemon is listening"),
        "{output:?}"
    );
}
