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
    child
        .stdin
        .take()
        .expect("the tool has standard input")
        .write_all(input.as_bytes())
        .expect("writes the arguments");
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
