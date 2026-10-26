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
    let output = run(&["status"]);
    assert_eq!(output.status.code(), Some(64));
    assert_eq!(stdout(&output), "");
    let message = stderr(&output);
    assert!(message.contains("not available in this build"), "{message}");
    assert!(message.contains("bus.sock"), "{message}");

    let object = run(&["status", "--json"]);
    assert_eq!(object.status.code(), Some(64));
    let parsed: serde_json::Value =
        serde_json::from_str(stderr(&object).trim_end()).expect("one json object");
    assert_eq!(parsed["error"]["code"], 64);
    assert_eq!(parsed["error"]["class"], "unsupported");
}

#[test]
fn a_message_the_operator_quoted_is_not_read_as_a_flag() {
    let output = run(&["send", "--branch", BRANCH, "--", "--wait"]);
    assert_eq!(output.status.code(), Some(64), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("`agentctl send`"),
        "the message parsed as a message, so the command was recognised"
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
fn help_is_text_on_stdout_and_exits_zero() {
    let output = run(&["--help"]);
    assert_eq!(output.status.code(), Some(0));
    let text = stdout(&output);
    assert!(text.contains("agentctl"), "{text}");
    assert!(text.contains("verify"), "{text}");
    assert_eq!(stderr(&output), "");
}
