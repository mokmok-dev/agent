//! What the kernel does with the bounds, against the real launcher.
//!
//! These are the only tests that answer the question the manifest asks: not what
//! the argv says, but what a process can do once it is running inside it.

#![allow(
    clippy::expect_used,
    reason = "a test reports a broken assumption by panicking"
)]

use std::path::PathBuf;
use std::process::{Command, Output};

/// The launcher cargo built for this test, which is the one the daemon ships.
const LAUNCHER: &str = env!("CARGO_BIN_EXE_agent-confine");

/// A directory that is removed when the test ends.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("agent-bounds-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("creates the scratch directory");
        Self(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn run(
    bounds: &[&str],
    program: &[&str],
) -> Output {
    Command::new(LAUNCHER)
        .args(bounds)
        .arg("--")
        .args(program)
        .output()
        .expect("runs the launcher")
}

fn said(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

const MEMORY: &str = "--memory-bytes=268435456";
const FILE: &str = "--file-bytes=8388608";

#[test]
fn a_tool_writes_where_its_manifest_says_and_nowhere_else() {
    let allowed = Scratch::new("allowed");
    let elsewhere = Scratch::new("elsewhere");
    let script = format!(
        "echo here > {}/written; echo there > {}/written 2>/dev/null || echo refused",
        allowed.0.display(),
        elsewhere.0.display()
    );
    let named = format!("--write={}", allowed.0.display());
    let output = run(
        &[MEMORY, FILE, "--network=none", &named],
        &["sh", "-c", &script],
    );
    assert!(output.status.success(), "{}", said(&output));
    assert!(said(&output).contains("refused"), "{}", said(&output));
    assert!(
        allowed.0.join("written").is_file(),
        "the directory the manifest named stayed unwritable"
    );
    assert!(
        !elsewhere.0.join("written").exists(),
        "the tool wrote outside the directory its manifest names"
    );
}

#[test]
fn a_tool_reads_inside_the_roots_and_nowhere_else() {
    let visible = Scratch::new("read-visible");
    let file = visible.0.join("visible");
    std::fs::write(&file, b"seen").expect("writes the visible file");
    let hidden = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let script = format!(
        "for path in (r'{}', r'{}'):\n    try:\n        print(path, open(path).read()[:4])\n    except OSError:\n        print(path, 'refused')",
        file.display(),
        hidden.display()
    );
    let output = run(
        &[MEMORY, FILE, "--network=none"],
        &["python3", "-c", &script],
    );
    let said = said(&output);
    assert!(said.contains(&format!("{} seen", file.display())), "{said}");
    assert!(
        said.contains(&format!("{} refused", hidden.display())),
        "a file outside the read roots was readable: {said}"
    );
}

#[test]
fn a_write_is_refused_even_where_the_daemon_could_write() {
    // The daemon's own state directory is the case that matters: a tool that can
    // edit the log is a tool that can rewrite history.
    let store = Scratch::new("store");
    let target = store.0.join("agent.db");
    std::fs::write(&target, b"events").expect("writes the file");
    let script = format!("echo overwritten > {} || true", target.display());
    let output = run(&[MEMORY, FILE, "--network=none"], &["sh", "-c", &script]);
    assert!(
        said(&output).contains("Permission denied") || !output.status.success(),
        "the tool overwrote a file it was not given: {}",
        said(&output)
    );
    assert_eq!(
        std::fs::read(&target).expect("still there"),
        b"events",
        "the file changed under a bound that should have refused the write"
    );
}

#[test]
fn a_tool_with_no_network_cannot_reach_one() {
    let output = run(
        &[MEMORY, FILE, "--network=none"],
        &[
            "python3",
            "-c",
            "import socket\ntry:\n    socket.socket(socket.AF_INET, socket.SOCK_STREAM)\n    print('open')\nexcept OSError:\n    print('refused')",
        ],
    );
    assert!(
        said(&output).contains("refused") && !said(&output).contains("open"),
        "a socket was opened under network=none: {}",
        said(&output)
    );
}

#[test]
fn a_tool_that_was_given_the_network_can_have_one() {
    let output = run(
        &[
            "--memory-bytes=268435456",
            "--file-bytes=8388608",
            "--network=any",
        ],
        &[
            "python3",
            "-c",
            "import socket\nsocket.socket(socket.AF_INET, socket.SOCK_STREAM).close()\nprint('open')",
        ],
    );
    assert!(
        said(&output).contains("open"),
        "the manifest said any and the socket was still refused: {}",
        said(&output)
    );
}

#[test]
fn a_tool_cannot_take_more_memory_than_its_manifest_allows() {
    let output = run(
        &["--memory-bytes=33554432", FILE, "--network=none"],
        &[
            "python3",
            "-c",
            "block = bytearray(120 * 1024 * 1024)\nprint('allocated', len(block))",
        ],
    );
    assert!(
        !said(&output).contains("allocated"),
        "the tool allocated past its bound: {}",
        said(&output)
    );
}

#[test]
fn a_tool_cannot_write_a_file_larger_than_its_manifest_allows() {
    let scratch = Scratch::new("bounded-file");
    let big = scratch.0.join("big");
    let script = format!("head -c 4194304 /dev/zero > {}", big.display());
    let output = run(
        &[MEMORY, "--file-bytes=65536", "--network=none"],
        &["sh", "-c", &script],
    );
    let written = std::fs::metadata(&big).map_or(0, |meta| meta.len());
    assert!(
        written <= 65536,
        "the file grew to {written} bytes past a bound of 65536: {}",
        said(&output)
    );
}

#[test]
fn a_tool_can_still_redirect_to_dev_null() {
    // Every shell script ends a line with 2>/dev/null, and a tool that cannot do it
    // reports failures it did not have.
    let output = run(
        &[MEMORY, FILE, "--network=none"],
        &["sh", "-c", "echo noise > /dev/null 2>&1; echo done"],
    );
    assert!(said(&output).contains("done"), "{}", said(&output));
    assert!(
        !said(&output).contains("Permission denied"),
        "/dev/null was refused: {}",
        said(&output)
    );
}

#[test]
fn a_program_that_is_a_tool_still_runs() {
    let output = run(&[MEMORY, FILE, "--network=none"], &["sh", "-c", "echo ran"]);
    assert_eq!(output.status.code(), Some(0), "{}", said(&output));
    assert!(said(&output).contains("ran"), "{}", said(&output));
}

#[test]
fn the_launcher_refuses_a_bound_this_kernel_cannot_hold() {
    // The check mode installs every bound and exits, so the daemon can refuse a
    // host before the first call rather than after it.
    let output = Command::new(LAUNCHER)
        .arg("--check")
        .arg(MEMORY)
        .arg(FILE)
        .arg("--network=none")
        .arg("--")
        .output()
        .expect("runs the launcher");
    assert_eq!(
        output.status.code(),
        Some(0),
        "this host cannot hold the bounds: {}",
        said(&output)
    );
}

#[test]
fn a_bound_the_daemon_did_not_write_is_refused_by_name() {
    let output = run(&["--network=none"], &["sh", "-c", "echo ran"]);
    assert_ne!(output.status.code(), Some(0), "{}", said(&output));
    assert!(
        said(&output).contains("--file-bytes is missing"),
        "{}",
        said(&output)
    );
    assert!(
        !said(&output).contains("ran"),
        "the program ran without its bounds"
    );
}
