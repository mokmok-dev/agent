//! The sandbox instance: policy, virtual filesystem, executor, and the
//! permission event flow bound together.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Instant;

use agentd_events::EventLog;
use uuid::Uuid;

use crate::error::SandboxError;
use crate::events;
use crate::executor::{ConfinedProcessExecutor, DenialReason, ExecResult, Executor};
use crate::policy::Policy;
use crate::shell;
use crate::vfs::MountedVfs;

/// A running sandbox: an agent's confinement layer for shell commands.
///
/// Every [`Sandbox::exec`] durably appends the full decision trail to the log —
/// `requested`, then `granted` or `denied`, then `exec.completed` — so audit
/// trails and approval UIs are ordinary subscribers, and no decision can be
/// lost.
pub struct Sandbox {
    id: Uuid,
    agent_id: String,
    policy: Policy,
    vfs: MountedVfs,
    executor: Arc<dyn Executor>,
    log: EventLog,
    executed: AtomicU32,
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
        policy: Policy,
        log: EventLog,
        agent_id: impl Into<String>,
    ) -> Result<Self, SandboxError> {
        let executor = ConfinedProcessExecutor::new(&policy)?;
        Self::with_executor(policy, log, agent_id, Arc::new(executor))
    }

    /// Creates a sandbox that delegates execution to `executor`.
    ///
    /// # Errors
    ///
    /// Fails closed with [`SandboxError::InvalidPolicy`] when the policy's
    /// filesystem domain cannot be assembled into a virtual filesystem.
    pub fn with_executor(
        policy: Policy,
        log: EventLog,
        agent_id: impl Into<String>,
        executor: Arc<dyn Executor>,
    ) -> Result<Self, SandboxError> {
        let vfs = MountedVfs::from_policy(&policy.fs)?;
        Ok(Self {
            id: Uuid::now_v7(),
            agent_id: agent_id.into(),
            policy,
            vfs,
            executor,
            log,
            executed: AtomicU32::new(0),
        })
    }

    /// The sandbox id, correlating its events in the log.
    #[must_use]
    pub const fn id(&self) -> Uuid {
        self.id
    }

    /// The shared virtual filesystem this sandbox sees.
    #[must_use]
    pub const fn vfs(&self) -> &MountedVfs {
        &self.vfs
    }

    /// The event log this sandbox publishes on and subscribes to.
    #[must_use]
    pub const fn log(&self) -> &EventLog {
        &self.log
    }

    /// Evaluates the command against the policy and, when allowed, runs it
    /// through the executor.
    ///
    /// Denials short-circuit: nothing is spawned, and the structured refusal
    /// is returned in [`ExecResult::denied_by`] and appended to the log.
    ///
    /// # Errors
    ///
    /// Returns [`SandboxError::Publish`] when a decision event cannot be
    /// durably appended. The `requested`/`granted` decision is appended before
    /// the command runs, so a command never starts without its decision
    /// recorded; a failure to record `exec.completed` after it ran is still
    /// surfaced as an error.
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

        // The counter is monotonic: denials consume budget too, which keeps
        // the guard race-free and stops a denied-command loop from spinning.
        let used = self.executed.fetch_add(1, Ordering::Relaxed);
        let denial = if used >= self.policy.limits.max_command_count {
            Some(DenialReason::CommandCountExceeded)
        } else if !shell::is_allowed(command, &self.policy.shell.allow) {
            Some(DenialReason::CommandNotAllowed)
        } else {
            None
        };

        if let Some(reason) = denial {
            self.log
                .publish(events::permission_denied(
                    &sandbox_id,
                    &self.agent_id,
                    command,
                ))
                .await?;
            return Ok(ExecResult::denied(reason));
        }

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
                result.denied_by.as_ref(),
            ))
            .await?;
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::Sandbox;
    use crate::error::SandboxError;
    use crate::executor::{DenialReason, ExecResult, Executor};
    use crate::policy::{CommandPrefix, Limits, Policy};
    use crate::vfs::Vfs as _;
    use agentd_events::{Event, EventLog, LogEntry};
    use async_trait::async_trait;
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

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
                denied_by: None,
            }
        }
    }

    fn policy(
        allow: &[&str],
        max_command_count: u32,
    ) -> Policy {
        Policy {
            shell: crate::policy::ShellPolicy {
                allow: allow
                    .iter()
                    .map(|prefix| CommandPrefix::new(*prefix))
                    .collect(),
                ..crate::policy::ShellPolicy::default()
            },
            limits: Limits {
                max_command_count,
                ..Limits::default()
            },
            ..Policy::default()
        }
    }

    fn open_log(dir: &Path) -> EventLog {
        EventLog::open(dir.join("events.jsonl")).expect("log should open")
    }

    fn sandbox(policy: Policy) -> (Sandbox, Arc<RecordingExecutor>, tempfile::TempDir) {
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
        let (sandbox, executor, _dir) = sandbox(policy(&["cargo test"], 10));
        let mut subscriber = sandbox.log().subscribe();

        let result = sandbox
            .exec("cargo test --workspace")
            .await
            .expect("exec should succeed");

        assert!(!result.is_denied());
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

    #[tokio::test]
    async fn denial_short_circuits_without_spawning() {
        let (sandbox, executor, _dir) = sandbox(policy(&["cargo test"], 10));
        let mut subscriber = sandbox.log().subscribe();

        let result = sandbox
            .exec("curl example.com")
            .await
            .expect("exec should succeed");

        assert_eq!(result.exit_code, 126);
        assert_eq!(result.denied_by, Some(DenialReason::CommandNotAllowed));
        assert!(result.stdout.is_empty());
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
        assert_eq!(events[1].data["decision"], "denied");
        assert!(
            !events[1]
                .data
                .as_object()
                .expect("object")
                .contains_key("exit_code")
        );
    }

    #[tokio::test]
    async fn command_count_guard_denies_beyond_the_limit() {
        let (sandbox, executor, _dir) = sandbox(policy(&["ls"], 2));
        let mut subscriber = sandbox.log().subscribe();

        sandbox.exec("ls").await.expect("exec should succeed");
        sandbox.exec("ls").await.expect("exec should succeed");
        let third = sandbox.exec("ls").await.expect("exec should succeed");

        assert_eq!(third.denied_by, Some(DenialReason::CommandCountExceeded));
        assert_eq!(executor.count(), 2);

        let events = drain(&mut subscriber);
        let types: Vec<&str> = events.iter().map(|event| event.r#type.as_str()).collect();
        assert_eq!(
            types,
            [
                crate::events::PERMISSION_REQUESTED,
                crate::events::PERMISSION_GRANTED,
                crate::events::EXEC_COMPLETED,
                crate::events::PERMISSION_REQUESTED,
                crate::events::PERMISSION_GRANTED,
                crate::events::EXEC_COMPLETED,
                crate::events::PERMISSION_REQUESTED,
                crate::events::PERMISSION_DENIED,
            ]
        );
    }

    #[tokio::test]
    async fn malformed_commands_are_denied() {
        let (sandbox, executor, _dir) = sandbox(policy(&["echo"], 10));

        let substitution = sandbox
            .exec("echo $(rm -rf /)")
            .await
            .expect("exec should succeed");
        assert_eq!(
            substitution.denied_by,
            Some(DenialReason::CommandNotAllowed)
        );

        let smuggled = sandbox
            .exec("echo ok; curl example.com")
            .await
            .expect("exec should succeed");
        assert_eq!(smuggled.denied_by, Some(DenialReason::CommandNotAllowed));
        assert_eq!(executor.count(), 0);
    }

    #[tokio::test]
    async fn a_third_party_granted_event_does_not_overrule_a_static_denial() {
        // An approver publishing `granted` must not widen the static policy:
        // deny wins without an explicit approval protocol.
        let (sandbox, executor, _dir) = sandbox(policy(&["cargo test"], 10));
        let mut subscriber = sandbox.log().subscribe();

        let publisher_log = sandbox.log().clone();
        let sandbox_id = sandbox.id().to_string();
        let approver = tokio::spawn(async move {
            while let Ok(recorded) = subscriber.recv().await {
                let event = recorded.event;
                if event.r#type == crate::events::PERMISSION_REQUESTED
                    && event.data["sandbox_id"] == sandbox_id
                {
                    let _ = publisher_log
                        .publish(Event::new(
                            crate::events::PERMISSION_GRANTED,
                            event.data.clone(),
                        ))
                        .await;
                }
            }
        });

        let result = sandbox
            .exec("curl example.com")
            .await
            .expect("exec should succeed");

        assert_eq!(result.denied_by, Some(DenialReason::CommandNotAllowed));
        assert_eq!(executor.count(), 0);
        approver.abort();
    }

    #[test]
    fn invalid_policy_fails_construction() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let bad = Policy {
            fs: crate::policy::FsPolicy {
                hide: vec![crate::policy::Pattern::new("[unclosed")],
                ..crate::policy::FsPolicy::default()
            },
            ..Policy::default()
        };

        assert!(matches!(
            Sandbox::with_executor(
                bad,
                open_log(dir.path()),
                "coder-1",
                RecordingExecutor::new()
            ),
            Err(SandboxError::InvalidPolicy(_))
        ));
    }

    #[test]
    fn vfs_is_built_from_the_policy() {
        let (sandbox, _executor, _dir) = sandbox(Policy::default());
        let anything = crate::VPath::root().join("anything");

        assert_eq!(
            sandbox
                .vfs()
                .read(&anything)
                .expect_err("nothing mounted")
                .kind(),
            std::io::ErrorKind::NotFound
        );
    }

    #[tokio::test]
    async fn zero_command_count_denies_everything() {
        let (sandbox, executor, _dir) = sandbox(policy(&["ls"], 0));

        let result = sandbox.exec("ls").await.expect("exec should succeed");

        assert_eq!(result.denied_by, Some(DenialReason::CommandCountExceeded));
        assert_eq!(executor.count(), 0);
    }
}
