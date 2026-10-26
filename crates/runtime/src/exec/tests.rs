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
        argv: argv.iter().map(|part| (*part).to_string()).collect(),
        idempotent: true,
        max_output,
        name: "test".to_string(),
        schema: "{}".to_string(),
        timeout: Duration::from_millis(timeout_ms),
    }
}

#[test]
fn a_tool_reads_the_arguments_it_was_given_on_stdin() {
    match run(
        &tool(&["cat"], 4096, 5000),
        r#"{"cmd":"date"}"#,
        Path::new("."),
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
fn a_tool_that_writes_more_than_the_bound_keeps_its_ends() {
    let long = "a".repeat(4096);
    match run(&tool(&["cat"], 100, 5000), &long, Path::new(".")) {
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
fn a_tool_that_outlives_its_bound_is_stopped_and_may_have_acted() {
    match run(&tool(&["sleep", "30"], 4096, 50), "", Path::new(".")) {
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
    ) {
        Outcome::Failed(_, happened) => assert!(!happened, "nothing started"),
        Outcome::Ran(_) => panic!("a missing program cannot run"),
    }
}

#[test]
fn a_tool_with_no_program_is_a_failure() {
    match run(&tool(&[], 4096, 5000), "", Path::new(".")) {
        Outcome::Failed(_, happened) => assert!(!happened),
        Outcome::Ran(_) => panic!("there is nothing to run"),
    }
}

#[test]
fn a_process_that_dies_by_a_signal_has_no_exit_code() {
    // A shell that kills itself is the cheapest way to reach the branch where the
    // status carries a signal instead of a code, and the log still needs a number.
    match run(
        &tool(&["sh", "-c", "kill -9 $$"], 4096, 5000),
        "",
        Path::new("."),
    ) {
        Outcome::Ran(ran) => assert_eq!(ran.status, -1),
        Outcome::Failed(class, happened) => panic!("unexpected {class:?} {happened}"),
    }
}
