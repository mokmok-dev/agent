//! The sandbox instance: policy, executor, and the permission event flow bound
//! together.

use std::sync::Arc;
use std::time::Instant;

use agentd_events::EventLog;
use uuid::Uuid;

use crate::error::SandboxError;
use crate::events;
use crate::executor::{ConfinedProcessExecutor, ExecResult, Executor};
use crate::policy::Policy;

/// A running sandbox: an agent's confinement layer for shell commands.
///
/// Every [`Sandbox::exec`] durably appends `requested`, `granted`, and the
/// terminal `exec.completed` to the log, so audit trails and approval UIs are
/// ordinary subscribers, and no decision can be lost. The confinement itself is
/// the OS profile the executor was built with; a command that the OS refuses
/// returns a non-zero exit code and a `sandbox.violation.*` classifier is the
/// follow-up.
pub struct Sandbox {
    id: Uuid,
    agent_id: String,
    executor: Arc<dyn Executor>,
    log: EventLog,
}

impl Sandbox {
    /// Creates a sandbox with the platform's layer-1 confined-process
    /// executor.
    ///
    /// # Errors
    ///
    /// Fails closed with [`SandboxError::InvalidPolicy`] when the policy is
    /// unusable and with [`SandboxError::UnsupportedPlatform`] when the
    /// platform has no confinement layer (see
    /// [`ConfinedProcessExecutor`](crate::ConfinedProcessExecutor)).
    pub fn new(
        policy: &Policy,
        log: EventLog,
        agent_id: impl Into<String>,
    ) -> Result<Self, SandboxError> {
        policy
            .validate()
            .map_err(|error| SandboxError::InvalidPolicy(error.to_string()))?;
        let executor = ConfinedProcessExecutor::new(policy)?;
        Ok(Self {
            id: Uuid::now_v7(),
            agent_id: agent_id.into(),
            executor: Arc::new(executor),
            log,
        })
    }

    /// Creates a sandbox that delegates execution to `executor`.
    ///
    /// # Errors
    ///
    /// Fails closed with [`SandboxError::InvalidPolicy`] when the policy is
    /// invalid.
    pub fn with_executor(
        policy: &Policy,
        log: EventLog,
        agent_id: impl Into<String>,
        executor: Arc<dyn Executor>,
    ) -> Result<Self, SandboxError> {
        policy
            .validate()
            .map_err(|error| SandboxError::InvalidPolicy(error.to_string()))?;
        Ok(Self {
            id: Uuid::now_v7(),
            agent_id: agent_id.into(),
            executor,
            log,
        })
    }

    /// The sandbox id, correlating its events in the log.
    #[must_use]
    pub const fn id(&self) -> Uuid {
        self.id
    }

    /// The event log this sandbox publishes on and subscribes to.
    #[must_use]
    pub const fn log(&self) -> &EventLog {
        &self.log
    }

    /// Records the decision and runs the command through the executor.
    ///
    /// The `requested` and `granted` events are appended before the command
    /// runs, so a command never starts without its decision recorded.
    ///
    /// # Errors
    ///
    /// Returns [`SandboxError::Publish`] when a decision or terminal event
    /// cannot be durably appended.
    pub async fn exec(
        &self,
        command: &str,
    ) -> Result<ExecResult, SandboxError> {
        let sandbox_id = self.id.to_string();
        self.log
            .publish(events::permission_requested(
                &sandbox_id,
                &self.agent_id,
                command,
            ))
            .await?;
        self.log
            .publish(events::permission_granted(
                &sandbox_id,
                &self.agent_id,
                command,
            ))
            .await?;
        let started = Instant::now();
        let result = self.executor.exec(command).await;
        self.log
            .publish(events::exec_completed(
                &sandbox_id,
                &self.agent_id,
                command,
                result.exit_code,
                u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                result.stdout.len() as u64,
                result.stderr.len() as u64,
            ))
            .await?;
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::Sandbox;
    use crate::error::SandboxError;
    use crate::executor::{ExecResult, Executor};
    use crate::policy::{Access, FsEntry, FsPolicy, Limits, Policy};
    use agentd_events::{Event, EventLog, LogEntry};
    use async_trait::async_trait;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    /// An executor that records how often it was invoked and returns canned
    /// output.
    struct RecordingExecutor {
        invocations: AtomicU32,
    }

    impl RecordingExecutor {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                invocations: AtomicU32::new(0),
            })
        }

        fn count(&self) -> u32 {
            self.invocations.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl Executor for RecordingExecutor {
        async fn exec(
            &self,
            _command: &str,
        ) -> ExecResult {
            self.invocations.fetch_add(1, Ordering::SeqCst);
            ExecResult {
                stdout: String::from("ok\n"),
                stderr: String::new(),
                exit_code: 0,
            }
        }
    }

    fn policy() -> Policy {
        Policy {
            fs: FsPolicy {
                entries: vec![FsEntry {
                    path: PathBuf::from("/repo"),
                    access: Access::Write,
                }],
                ..FsPolicy::default()
            },
            ..Policy::default()
        }
    }

    fn open_log(dir: &Path) -> EventLog {
        EventLog::open(dir.join("events.jsonl")).expect("log should open")
    }

    fn sandbox(policy: &Policy) -> (Sandbox, Arc<RecordingExecutor>, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let executor = RecordingExecutor::new();
        let sandbox =
            Sandbox::with_executor(policy, open_log(dir.path()), "coder-1", executor.clone())
                .expect("valid policy");
        (sandbox, executor, dir)
    }

    fn drain(receiver: &mut tokio::sync::broadcast::Receiver<LogEntry>) -> Vec<Event> {
        let mut collected = Vec::new();
        while let Ok(recorded) = receiver.try_recv() {
            collected.push(recorded.event);
        }
        collected
    }

    #[tokio::test]
    async fn exec_publishes_requested_granted_and_completed() {
        let (sandbox, executor, _dir) = sandbox(&policy());
        let mut subscriber = sandbox.log().subscribe();

        let result = sandbox
            .exec("cargo test --workspace")
            .await
            .expect("exec should succeed");

        assert_eq!(result.exit_code, 0);
        assert_eq!(result.stdout, "ok\n");
        assert_eq!(executor.count(), 1);

        let events = drain(&mut subscriber);
        let types: Vec<&str> = events.iter().map(|event| event.r#type.as_str()).collect();
        assert_eq!(
            types,
            [
                crate::events::PERMISSION_REQUESTED,
                crate::events::PERMISSION_GRANTED,
                crate::events::EXEC_COMPLETED,
            ]
        );
        for event in &events {
            assert_eq!(event.data["sandbox_id"], sandbox.id().to_string());
            assert_eq!(event.data["subject"], "cargo test --workspace");
        }
        assert_eq!(events[2].data["exit_code"], 0);
        assert!(events[2].data["duration_ms"].as_u64().is_some());
    }

    #[test]
    fn invalid_policy_fails_construction() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let bad = Policy {
            limits: Limits {
                timeout: Duration::ZERO,
                ..Limits::default()
            },
            ..Policy::default()
        };

        assert!(matches!(
            Sandbox::with_executor(
                &bad,
                open_log(dir.path()),
                "coder-1",
                RecordingExecutor::new()
            ),
            Err(SandboxError::InvalidPolicy(_))
        ));
    }
}
