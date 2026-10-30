//! What an operator or the daemon sees on the other side of the binary.

#![allow(
    clippy::expect_used,
    reason = "a test reports a broken assumption by panicking"
)]

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::Value;

const BINARY: &str = env!("CARGO_BIN_EXE_agent-tool");

struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "agent-tool-cli-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_nanos())
        ));
        std::fs::create_dir_all(&path).expect("creates the scratch directory");
        let path = std::fs::canonicalize(&path).expect("canonicalizes the scratch directory");
        Self { path }
    }

    fn write(
        &self,
        relative: &str,
        content: &str,
    ) {
        std::fs::write(self.path.join(relative), content).expect("writes the fixture");
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn call(
    arguments: &[&str],
    workspace: &Path,
    input: &str,
) -> Output {
    let mut child = Command::new(BINARY)
        .args(arguments)
        .current_dir(workspace)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("starts the tool binary");
    // A tool that refuses at startup never reads its input, and a closed pipe is
    // that refusal and not a failure of the test.
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(input.as_bytes());
    }
    child.wait_with_output().expect("waits for the tool")
}

fn answer(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "exit {:?}: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("the tool writes one JSON object")
}

#[test]
fn the_list_names_every_tool() {
    let scratch = Scratch::new();
    let output = call(&["--list"], &scratch.path, "");
    let text = String::from_utf8(output.stdout).expect("the list is text");
    let names: Vec<&str> = text
        .lines()
        .map(|line| line.split('\t').next().expect("a name"))
        .collect();
    assert_eq!(
        names,
        [
            "edit_file",
            "list_dir",
            "read_file",
            "run",
            "search",
            "write_file"
        ]
    );
}

#[test]
fn the_schema_is_the_one_the_provider_reads() {
    let scratch = Scratch::new();
    let output = call(&["--schema", "read_file"], &scratch.path, "");
    assert!(output.status.success());
    let schema: Value = serde_json::from_slice(&output.stdout).expect("the schema is JSON");
    assert_eq!(schema["type"], "object");
    assert_eq!(schema["additionalProperties"], false);
    assert!(schema["properties"]["path"].is_object());
}

#[test]
fn a_call_reads_stdin_and_writes_one_envelope() {
    let scratch = Scratch::new();
    scratch.write("note.txt", "hello\n");
    let value = answer(&call(
        &["read_file"],
        &scratch.path,
        r#"{"path":"note.txt"}"#,
    ));
    assert_eq!(value["ok"], true);
    assert_eq!(value["data"]["content"], "hello\n");
    assert_eq!(value["data"]["lines"], 1);
}

#[test]
fn a_refused_call_is_still_a_tool_answer() {
    let scratch = Scratch::new();
    let value = answer(&call(
        &["write_file"],
        &scratch.path,
        r#"{"path":"../escaped","content":"x"}"#,
    ));
    assert_eq!(value["ok"], false);
    assert_eq!(value["error"]["code"], "outside_workspace");
}

#[test]
fn a_key_the_schema_does_not_name_is_invalid_args() {
    let scratch = Scratch::new();
    let value = answer(&call(
        &["read_file"],
        &scratch.path,
        r#"{"path":"note.txt","extra":1}"#,
    ));
    assert_eq!(value["error"]["code"], "invalid_args");
}

#[test]
fn run_without_an_allowlist_refuses_every_program() {
    let scratch = Scratch::new();
    let value = answer(&call(&["run"], &scratch.path, r#"{"program":"sh"}"#));
    assert_eq!(value["error"]["code"], "program_not_allowed");
}

#[test]
fn run_starts_an_allowed_program() {
    let scratch = Scratch::new();
    let value = answer(&call(
        &["run", "--allow=sh"],
        &scratch.path,
        r#"{"program":"sh","args":["-c","echo hello"]}"#,
    ));
    assert_eq!(value["ok"], true);
    assert_eq!(value["data"]["stdout"], "hello\n");
    assert_eq!(value["data"]["status"], 0);
}

#[test]
fn a_relative_allow_entry_stops_the_tool_at_startup() {
    let scratch = Scratch::new();
    let output = call(&["run", "--allow=bin/sh"], &scratch.path, "{}");
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("relative path"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn an_unknown_tool_is_a_usage_error() {
    let scratch = Scratch::new();
    let output = call(&["fetch"], &scratch.path, "{}");
    assert!(!output.status.success());
}

#[test]
fn no_arguments_is_a_usage_error() {
    let scratch = Scratch::new();
    let output = call(&[], &scratch.path, "");
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("usage: agent-tool"), "{stderr}");
}

#[test]
fn an_unknown_flag_is_a_usage_error() {
    let scratch = Scratch::new();
    let output = call(&["--bogus"], &scratch.path, "");
    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("usage: agent-tool"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn a_schema_for_an_unknown_tool_is_a_usage_error() {
    let scratch = Scratch::new();
    let output = call(&["--schema", "fetch"], &scratch.path, "");
    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("there is no tool named fetch"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn a_tool_that_takes_no_program_list_refuses_one() {
    let scratch = Scratch::new();
    let output = call(&["read_file", "--allow=sh"], &scratch.path, "{}");
    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("does not take --allow=sh"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn an_input_that_cannot_be_read_is_a_failure_not_an_answer() {
    let scratch = Scratch::new();
    // A directory as standard input is the one descriptor a test can open that
    // fails on the first read.
    let input = std::fs::File::open(&scratch.path).expect("opens the directory");
    let output = Command::new(BINARY)
        .arg("read_file")
        .current_dir(&scratch.path)
        .stdin(Stdio::from(input))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("starts the tool binary");
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("cannot read standard input"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
