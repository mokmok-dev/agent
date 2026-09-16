//! End-to-end permission flow tests: the `requested` → `granted` →
//! `exec.completed` choreography over the durable event log, through the public
//! API.
//!
//! The helpers below use `expect` like the `#[cfg(test)]` modules in `src` do;
//! the workspace `allow-*-in-tests` clippy configuration cannot see integration
//! test files, so it is replicated here.

#![allow(clippy::expect_used, clippy::panic)]

use agentd_events::{EventLog, LogEntry};
use agentd_sandbox::{
    Access, ExecResult, Executor, FsEntry, FsPolicy, Policy, Sandbox, ShellPolicy,
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
            denied: false,
        }
    }
}

fn policy() -> Policy {
    Policy {
        fs: FsPolicy {
            entries: vec![FsEntry {
                path: std::path::PathBuf::from("/repo"),
                access: Access::Write,
            }],
            ..FsPolicy::default()
        },
        shell: ShellPolicy::default(),
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

fn collect_types(subscriber: &mut tokio::sync::broadcast::Receiver<LogEntry>) -> Vec<String> {
    let mut types = Vec::new();
    while let Ok(recorded) = subscriber.try_recv() {
        types.push(recorded.event.r#type);
    }
    types
}

#[tokio::test]
async fn a_command_flows_requested_granted_completed() {
    let dir = tempfile::tempdir().expect("tempdir should be created");
    let log = open_log(dir.path());
    let mut subscriber = log.subscribe();
    let sandbox =
        Sandbox::with_executor(&policy(), log, "coder-1", executor()).expect("valid policy");

    let result = sandbox
        .exec("cargo test --workspace")
        .await
        .expect("exec should succeed");

    assert_eq!(result.exit_code, 0);
    assert_eq!(result.stdout, "done\n");
    assert_eq!(
        collect_types(&mut subscriber),
        [
            agentd_sandbox::PERMISSION_REQUESTED,
            agentd_sandbox::PERMISSION_GRANTED,
            agentd_sandbox::EXEC_COMPLETED,
        ]
    );
}
