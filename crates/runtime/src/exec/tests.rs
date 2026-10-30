//! What a tool run does, against processes that are really started.

#![allow(
    clippy::expect_used,
    reason = "a test reports a broken assumption by panicking"
)]

use std::path::Path;
use std::time::Duration;

use super::{Outcome, run};
use crate::Tool;

fn tool(
    argv: &[&str],
    max_output: usize,
    timeout_ms: u64,
) -> Tool {
    Tool {
        description: String::new(),
        risk: agent_core::event::Risk::Read,
        argv: argv.iter().map(|part| (*part).to_string()).collect(),
        idempotent: true,
        max_output,
        name: "test".to_string(),
        schema: "{}".to_string(),
        timeout: Duration::from_millis(timeout_ms),
        env: Vec::new(),
        write: Vec::new(),
        network: crate::Network::Any,
        memory_bytes: 256 * 1024 * 1024,
        file_bytes: 8 * 1024 * 1024,
        confined: false,
    }
}

#[test]
fn a_tool_reads_the_arguments_it_was_given_on_stdin() {
    match run(
        &tool(&["cat"], 4096, 5000),
        r#"{"cmd":"date"}"#,
        Path::new("."),
        &mut || {},
    ) {
        Outcome::Ran(ran) => {
            assert_eq!(ran.output, r#"{"cmd":"date"}"#);
            assert_eq!(ran.status, 0);
            assert!(!ran.truncated);
        },
        Outcome::Failed(class, happened) => panic!("unexpected {class:?} {happened}"),
    }
}

#[test]
fn a_tool_that_declares_no_environment_sees_none() {
    // A launcher of its own is what a tool gets from the configuration; this fixture
    // runs without one, so what is left to see is what the executor passes.
    match run(
        &tool(&["env"], 4096, 5000),
        "",
        Path::new("/tmp"),
        &mut || {},
    ) {
        Outcome::Ran(ran) => assert!(
            ran.output.is_empty(),
            "the tool saw variables it never declared: {}",
            ran.output
        ),
        Outcome::Failed(class, happened) => panic!("unexpected {class:?} {happened}"),
    }
}

#[test]
fn a_tool_sees_only_the_variables_its_manifest_declares() {
    let mut declared = tool(&["env"], 4096, 5000);
    declared.env = vec!["PATH".to_string()];
    let withheld = ["HOME", "PWD", "USER"]
        .into_iter()
        .find(|name| std::env::var_os(name).is_some())
        .expect("a variable of the test process to withhold");
    match run(&declared, "", Path::new("."), &mut || {}) {
        Outcome::Ran(ran) => {
            assert!(ran.output.contains("PATH="), "{}", ran.output);
            assert!(
                !ran.output.contains(&format!("{withheld}=")),
                "{withheld} leaked to the tool: {}",
                ran.output
            );
        },
        Outcome::Failed(class, happened) => panic!("unexpected {class:?} {happened}"),
    }
}

#[test]
fn a_tool_that_writes_more_than_the_bound_keeps_its_ends() {
    let long = "a".repeat(4096);
    match run(
        &tool(&["cat"], 100, 5000),
        &long,
        Path::new("."),
        &mut || {},
    ) {
        Outcome::Ran(ran) => {
            assert!(ran.truncated, "the event would have carried everything");
            // Half the bound from each end, and the count of what was dropped
            // between them: 4096 bytes cut to 100 keeps 50 and 50 and drops 3996.
            assert_eq!(
                ran.output,
                format!("{}\n...3996\n{}", "a".repeat(50), "a".repeat(50))
            );
        },
        Outcome::Failed(class, happened) => panic!("unexpected {class:?} {happened}"),
    }
}

#[test]
fn a_tool_that_writes_more_than_the_pipe_holds_is_drained_and_finishes() {
    let mut writing = tool(&["sh", "-c", "head -c 200000 /dev/zero"], 4096, 5000);
    writing.env = vec!["PATH".to_string()];
    match run(&writing, "", Path::new("."), &mut || {}) {
        Outcome::Ran(ran) => {
            assert_eq!(ran.status, 0, "the tool was stopped before it finished");
            assert!(ran.truncated, "the event would have carried everything");
            assert!(ran.output.contains("..."), "the middle was not cut");
        },
        Outcome::Failed(class, happened) => panic!("unexpected {class:?} {happened}"),
    }
}

#[test]
fn a_running_tool_feeds_the_heartbeat() {
    let mut beats = 0;
    match run(
        &tool(&["sleep", "0.1"], 4096, 5000),
        "",
        Path::new("."),
        &mut || beats += 1,
    ) {
        Outcome::Ran(_) => assert!(
            beats > 0,
            "a long call never told the supervisor it was alive"
        ),
        Outcome::Failed(class, happened) => panic!("unexpected {class:?} {happened}"),
    }
}

#[test]
fn a_tool_that_outlives_its_bound_is_stopped_and_may_have_acted() {
    match run(
        &tool(&["sleep", "30"], 4096, 50),
        "",
        Path::new("."),
        &mut || {},
    ) {
        Outcome::Failed(class, happened) => {
            assert_eq!(class, agent_core::event::ErrorClass::Timeout);
            assert!(happened, "a stopped process may still have done something");
        },
        Outcome::Ran(_) => panic!("the wait was not bounded"),
    }
}

#[test]
fn a_program_that_does_not_exist_is_a_failure_and_nothing_happened() {
    match run(
        &tool(&["no-such-program-6f2a"], 4096, 5000),
        "",
        Path::new("."),
        &mut || {},
    ) {
        Outcome::Failed(_, happened) => assert!(!happened, "nothing started"),
        Outcome::Ran(_) => panic!("a missing program cannot run"),
    }
}

#[test]
fn a_tool_with_no_program_is_a_failure() {
    match run(&tool(&[], 4096, 5000), "", Path::new("."), &mut || {}) {
        Outcome::Failed(_, happened) => assert!(!happened),
        Outcome::Ran(_) => panic!("there is nothing to run"),
    }
}

#[test]
fn a_process_that_kills_itself_reports_the_signal_as_minus_one() {
    // A process the kernel stopped has no exit code, and -1 is how the event
    // records that absence instead of inventing a status a reader would take
    // for one the program chose.
    match run(
        &tool(&["sh", "-c", "kill -9 $$"], 4096, 5000),
        "",
        Path::new("."),
        &mut || {},
    ) {
        Outcome::Ran(ran) => assert_eq!(ran.status, -1),
        Outcome::Failed(class, happened) => panic!("unexpected {class:?} {happened}"),
    }
}

#[test]
fn the_head_keeps_one_byte_of_room_past_the_bound() {
    let mut head = Vec::new();
    super::keep_head(&mut head, b"ab", 2);
    assert_eq!(head, b"ab");
    super::keep_head(&mut head, b"c", 2);
    assert_eq!(
        head, b"abc",
        "the byte past the bound is what says there was more"
    );
    super::keep_head(&mut head, b"d", 2);
    assert_eq!(
        head, b"abc",
        "nothing more is kept once the bound is passed"
    );
}

#[test]
fn the_tail_keeps_the_last_half_of_a_long_chunk() {
    let mut tail = Vec::new();
    super::keep_tail(&mut tail, b"abcdef", 3);
    assert_eq!(tail, b"def");
}

#[test]
fn the_tail_drops_its_front_and_never_its_back() {
    let mut tail = Vec::new();
    super::keep_tail(&mut tail, b"abc", 4);
    assert_eq!(tail, b"abc");
    super::keep_tail(&mut tail, b"d", 4);
    assert_eq!(tail, b"abcd");
    super::keep_tail(&mut tail, b"ef", 4);
    assert_eq!(tail, b"cdef", "the front is dropped, not the back");
}

#[test]
fn a_zero_half_keeps_nothing() {
    let mut tail = b"kept".to_vec();
    super::keep_tail(&mut tail, b"abc", 0);
    assert_eq!(
        tail, b"kept",
        "a zero bound drops nothing and keeps nothing"
    );
}

#[test]
fn a_stopped_tool_is_no_longer_running() {
    let mut child = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .expect("starts a process to stop");
    super::stop(&mut child);
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(status) = child.try_wait().expect("waits") {
            assert!(!status.success(), "a stopped tool did not fail");
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the tool was not stopped"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
