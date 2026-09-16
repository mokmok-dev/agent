//! End-to-end permission flow tests: the `requested` → `granted`/`denied` →
//! `exec.completed` choreography over the durable event log, through the public
//! API.
//!
//! The helpers below use `expect` like the `#[cfg(test)]` modules in `src` do;
//! the workspace `allow-*-in-tests` clippy configuration cannot see integration
//! test files, so it is replicated here.

#![allow(clippy::expect_used, clippy::panic)]

use agentd_events::{Event, EventLog, LogEntry};
use agentd_integration_sandbox::{
    CommandPrefix, DenialReason, ExecResult, Executor, Limits, Policy, Sandbox, ShellPolicy,
};
use async_trait::async_trait;

struct RecordingExecutor {
    invocations: std::sync::atomic::AtomicU32,
}

#[async_trait]
impl Executor for RecordingExecutor {
    async fn exec(
        &self,
        _command: &str,
    ) -> ExecResult {
        self.invocations
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        ExecResult {
            stdout: String::from("done\n"),
            stderr: String::new(),
            exit_code: 0,
            denied_by: None,
        }
    }
}

fn policy() -> Policy {
    Policy {
        shell: ShellPolicy {
            allow: vec![CommandPrefix::new("cargo test")],
            ..ShellPolicy::default()
        },
        limits: Limits {
            max_command_count: 5,
            ..Limits::default()
        },
        ..Policy::default()
    }
}

fn open_log(dir: &std::path::Path) -> EventLog {
    EventLog::open(dir.join("events.jsonl")).expect("log should open")
}

fn executor() -> std::sync::Arc<RecordingExecutor> {
    std::sync::Arc::new(RecordingExecutor {
        invocations: std::sync::atomic::AtomicU32::new(0),
    })
}

fn collect_kinds(subscriber: &mut tokio::sync::broadcast::Receiver<LogEntry>) -> Vec<String> {
    let mut kinds = Vec::new();
    while let Ok(recorded) = subscriber.try_recv() {
        kinds.push(recorded.event.kind);
    }
    kinds
}

#[tokio::test]
async fn allowed_command_flows_requested_granted_completed() {
    let dir = tempfile::tempdir().expect("tempdir should be created");
    let log = open_log(dir.path());
    let mut subscriber = log.subscribe();
    let sandbox =
        Sandbox::with_executor(policy(), log, "coder-1", executor()).expect("valid policy");

    let result = sandbox
        .exec("cargo test --workspace")
        .await
        .expect("exec should succeed");

    assert!(!result.is_denied());
    assert_eq!(
        collect_kinds(&mut subscriber),
        [
            agentd_integration_sandbox::PERMISSION_REQUESTED,
            agentd_integration_sandbox::PERMISSION_GRANTED,
            agentd_integration_sandbox::EXEC_COMPLETED,
        ]
    );
}

#[tokio::test]
async fn denied_command_flows_requested_denied_and_spawns_nothing() {
    let dir = tempfile::tempdir().expect("tempdir should be created");
    let log = open_log(dir.path());
    let mut subscriber = log.subscribe();
    let executor = executor();
    let sandbox =
        Sandbox::with_executor(policy(), log, "coder-1", executor.clone()).expect("valid policy");

    let result = sandbox
        .exec("curl example.com")
        .await
        .expect("exec should succeed");

    assert_eq!(result.denied_by, Some(DenialReason::CommandNotAllowed));
    assert_eq!(result.exit_code, 126);
    assert_eq!(
        executor
            .invocations
            .load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    assert_eq!(
        collect_kinds(&mut subscriber),
        [
            agentd_integration_sandbox::PERMISSION_REQUESTED,
            agentd_integration_sandbox::PERMISSION_DENIED,
        ]
    );
}

#[tokio::test]
async fn deny_wins_over_a_forged_granted_event() {
    let dir = tempfile::tempdir().expect("tempdir should be created");
    let log = open_log(dir.path());
    let mut subscriber = log.subscribe();
    let sandbox =
        Sandbox::with_executor(policy(), log.clone(), "coder-1", executor()).expect("valid policy");

    // A WS-client "approver" that grants everything it sees. The static
    // evaluator must not care: deny wins in this version.
    let approver = tokio::spawn(async move {
        loop {
            match subscriber.recv().await {
                Ok(recorded)
                    if recorded.event.kind == agentd_integration_sandbox::PERMISSION_REQUESTED =>
                {
                    let _ = log
                        .publish(Event::new(
                            agentd_integration_sandbox::PERMISSION_GRANTED,
                            recorded.event.data,
                        ))
                        .await;
                },
                Ok(_) => {},
                Err(
                    tokio::sync::broadcast::error::RecvError::Lagged(_)
                    | tokio::sync::broadcast::error::RecvError::Closed,
                ) => break,
            }
        }
    });

    let result = sandbox
        .exec("curl example.com")
        .await
        .expect("exec should succeed");

    assert_eq!(result.denied_by, Some(DenialReason::CommandNotAllowed));
    approver.abort();
}
