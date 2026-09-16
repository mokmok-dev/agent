//! The sandbox instance: policy, executor, approval, and the permission event
//! flow bound together.

use std::sync::Arc;
use std::time::{Duration, Instant};

use agentd_events::{EventLog, LogEntry};
use tokio::sync::broadcast::error::RecvError;
use uuid::Uuid;

use crate::error::SandboxError;
use crate::events::{self, DECISION_AUTO, DECISION_PENDING, PERMISSION_DENIED, PERMISSION_GRANTED};
use crate::executor::{ConfinedProcessExecutor, ExecResult, Executor};
use crate::policy::Policy;
use crate::violation;

/// How a sandbox decides whether a command may run.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Approval {
    /// A static rule grants immediately; no approver is awaited.
    Auto,
    /// The request is held `pending` until an approver publishes a matching
    /// `sandbox.permission.granted`/`denied` (correlated by `request_id`), or
    /// `timeout` expires, which denies.
    Required {
        /// How long to wait for a decision before denying.
        timeout: Duration,
    },
}

/// A running sandbox: an agent's confinement layer for shell commands.
///
/// Every [`Sandbox::exec`] durably appends the decision trail to the log —
/// `requested`, then `granted` or `denied`, then `exec.completed` — so audit
/// trails and approval UIs are ordinary subscribers, and no decision can be
/// lost. The confinement itself is the OS profile the executor was built with.
pub struct Sandbox {
    id: Uuid,
    agent_id: String,
    executor: Arc<dyn Executor>,
    log: EventLog,
    approval: Approval,
}

impl Sandbox {
    /// Creates a sandbox with the platform's layer-1 confined-process executor
    /// and automatic (static) approval.
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
            approval: Approval::Auto,
        })
    }

    /// Creates a sandbox that delegates execution to `executor` with automatic
    /// approval.
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
            approval: Approval::Auto,
        })
    }

    /// Sets the approval mode, e.g. to require a human approver.
    #[must_use]
    pub const fn with_approval(
        mut self,
        approval: Approval,
    ) -> Self {
        self.approval = approval;
        self
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

    /// Records the decision and, when granted, runs the command.
    ///
    /// With [`Approval::Auto`] a static rule grants immediately. With
    /// [`Approval::Required`] the request is published `pending` and this
    /// awaits an approver's decision, correlated by `request_id`; a denial or
    /// timeout returns a denied [`ExecResult`] without spawning anything.
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
        let request_id = Uuid::now_v7().to_string();

        match self.approval {
            Approval::Auto => {
                self.publish(events::permission_requested(
                    &sandbox_id,
                    &request_id,
                    &self.agent_id,
                    command,
                    DECISION_AUTO,
                ))
                .await?;
                self.publish(events::permission_granted(
                    &sandbox_id,
                    &request_id,
                    &self.agent_id,
                    command,
                ))
                .await?;
            },
            Approval::Required { timeout } => {
                // Subscribe before publishing, so the approver's decision
                // cannot be missed between the request and the wait.
                let mut decisions = self.log.subscribe();
                self.publish(events::permission_requested(
                    &sandbox_id,
                    &request_id,
                    &self.agent_id,
                    command,
                    DECISION_PENDING,
                ))
                .await?;
                match await_decision(&mut decisions, &request_id, timeout).await {
                    // The approver's `granted` is already the durable record.
                    Decision::Granted => {},
                    // The approver's `denied` is already the durable record.
                    Decision::Denied => {
                        return Ok(ExecResult::denied("the command was not approved"));
                    },
                    // No approver answered, so the sandbox records the denial.
                    Decision::TimedOut => {
                        self.publish(events::permission_denied(
                            &sandbox_id,
                            &request_id,
                            &self.agent_id,
                            command,
                        ))
                        .await?;
                        return Ok(ExecResult::denied("the approval request timed out"));
                    },
                }
            },
        }

        let started = Instant::now();
        let result = self.executor.exec(command).await;
        if let Some(violation) = violation::classify_violation(&result) {
            self.publish(violation::violation_event(
                &sandbox_id,
                &request_id,
                &self.agent_id,
                command,
                &violation,
            ))
            .await?;
        }
        self.publish(events::exec_completed(
            &sandbox_id,
            &request_id,
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

    /// Durably appends `event`, surfacing a failure as [`SandboxError::Publish`].
    async fn publish(
        &self,
        event: agentd_events::Event,
    ) -> Result<(), SandboxError> {
        self.log.publish(event).await?;
        Ok(())
    }
}

/// The outcome of waiting for an approver's decision.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Decision {
    /// An approver granted the request.
    Granted,
    /// An approver denied the request.
    Denied,
    /// No decision arrived within the timeout.
    TimedOut,
}

/// Waits for a decision on `request_id`.
///
/// An unrelated event is ignored; a lagged subscriber keeps waiting; the bus
/// closing or the timeout is a denial. A decision is trusted only when it
/// carries the exact `request_id`, so a grant for another request cannot
/// release this one.
async fn await_decision(
    receiver: &mut tokio::sync::broadcast::Receiver<LogEntry>,
    request_id: &str,
    timeout: Duration,
) -> Decision {
    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            () = &mut deadline => return Decision::TimedOut,
            incoming = receiver.recv() => match incoming {
                Ok(recorded) => {
                    let event = recorded.event;
                    let matches = event
                        .data
                        .get("request_id")
                        .and_then(serde_json::Value::as_str)
                        == Some(request_id);
                    if !matches {
                        continue;
                    }
                    match event.r#type.as_str() {
                        PERMISSION_GRANTED => return Decision::Granted,
                        PERMISSION_DENIED => return Decision::Denied,
                        _ => {},
                    }
                },
                Err(RecvError::Lagged(_)) => {},
                Err(RecvError::Closed) => return Decision::Denied,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Approval, Sandbox};
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
                denied: false,
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
        let request_id = events[0].data["request_id"]
            .as_str()
            .expect("a request id")
            .to_string();
        for event in &events {
            assert_eq!(event.data["sandbox_id"], sandbox.id().to_string());
            assert_eq!(event.data["request_id"], request_id);
            assert_eq!(event.data["subject"], "cargo test --workspace");
        }
        assert_eq!(events[2].data["exit_code"], 0);
    }

    /// Spawns an approver that grants or denies the first request it sees.
    fn spawn_approver(
        log: &EventLog,
        decision: &'static str,
    ) -> tokio::task::JoinHandle<()> {
        let mut receiver = log.subscribe();
        let publisher = log.clone();
        tokio::spawn(async move {
            while let Ok(recorded) = receiver.recv().await {
                let event = recorded.event;
                let matches = event
                    .data
                    .get("request_id")
                    .and_then(serde_json::Value::as_str);
                if event.r#type == crate::events::PERMISSION_REQUESTED
                    && let Some(request_id) = matches
                {
                    let sandbox_id = event.data["sandbox_id"].as_str().unwrap_or_default();
                    let agent_id = event.data["agent_id"].as_str().unwrap_or_default();
                    let subject = event.data["subject"].as_str().unwrap_or_default();
                    let reply = if decision == "grant" {
                        crate::events::permission_granted(sandbox_id, request_id, agent_id, subject)
                    } else {
                        crate::events::permission_denied(sandbox_id, request_id, agent_id, subject)
                    };
                    let _ = publisher.publish(reply).await;
                    return;
                }
            }
        })
    }

    #[tokio::test]
    async fn required_approval_runs_when_an_approver_grants() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let log = open_log(dir.path());
        let executor = RecordingExecutor::new();
        let sandbox = Sandbox::with_executor(&policy(), log.clone(), "coder-1", executor.clone())
            .expect("valid policy")
            .with_approval(Approval::Required {
                timeout: Duration::from_secs(5),
            });
        let mut subscriber = sandbox.log().subscribe();
        let approver = spawn_approver(&log, "grant");

        let result = sandbox.exec("ls").await.expect("exec should succeed");

        assert_eq!(executor.count(), 1);
        assert!(!result.is_denied());
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
        assert_eq!(events[0].data["decision"], crate::events::DECISION_PENDING);
        approver.await.expect("approver should finish");
    }

    #[tokio::test]
    async fn required_approval_denies_and_spawns_nothing() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let log = open_log(dir.path());
        let executor = RecordingExecutor::new();
        let sandbox = Sandbox::with_executor(&policy(), log.clone(), "coder-1", executor.clone())
            .expect("valid policy")
            .with_approval(Approval::Required {
                timeout: Duration::from_secs(5),
            });
        let mut subscriber = sandbox.log().subscribe();
        let approver = spawn_approver(&log, "deny");

        let result = sandbox.exec("ls").await.expect("exec should succeed");

        assert!(result.is_denied());
        assert_eq!(executor.count(), 0);
        let events = drain(&mut subscriber);
        let types: Vec<&str> = events.iter().map(|event| event.r#type.as_str()).collect();
        assert_eq!(
            types,
            [
                crate::events::PERMISSION_REQUESTED,
                crate::events::PERMISSION_DENIED,
            ]
        );
        approver.await.expect("approver should finish");
    }

    #[tokio::test]
    async fn required_approval_times_out_and_denies() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let executor = RecordingExecutor::new();
        let sandbox =
            Sandbox::with_executor(&policy(), open_log(dir.path()), "coder-1", executor.clone())
                .expect("valid policy")
                .with_approval(Approval::Required {
                    timeout: Duration::from_millis(50),
                });

        let result = sandbox.exec("ls").await.expect("exec should succeed");

        assert!(result.is_denied());
        assert_eq!(executor.count(), 0);
    }

    /// An executor that always reports an OS denial.
    struct DenyingExecutor;

    #[async_trait]
    impl Executor for DenyingExecutor {
        async fn exec(
            &self,
            _command: &str,
        ) -> ExecResult {
            ExecResult {
                stdout: String::new(),
                stderr: String::from("touch: /etc/blocked: Operation not permitted"),
                exit_code: 1,
                denied: false,
            }
        }
    }

    #[tokio::test]
    async fn an_os_denial_publishes_a_violation_event() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let log = open_log(dir.path());
        let mut subscriber = log.subscribe();
        let sandbox = Sandbox::with_executor(&policy(), log, "coder-1", Arc::new(DenyingExecutor))
            .expect("valid policy");

        let result = sandbox
            .exec("touch /etc/blocked")
            .await
            .expect("exec should succeed");

        assert_ne!(result.exit_code, 0);
        let events = drain(&mut subscriber);
        let types: Vec<&str> = events.iter().map(|event| event.r#type.as_str()).collect();
        assert_eq!(
            types,
            [
                crate::events::PERMISSION_REQUESTED,
                crate::events::PERMISSION_GRANTED,
                crate::violation::VIOLATION_FILESYSTEM,
                crate::events::EXEC_COMPLETED,
            ]
        );
        assert_eq!(events[2].data["reason"], "operation_not_permitted");
        assert_eq!(events[2].data["path"], "/etc/blocked");
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
