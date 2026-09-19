//! The sandbox instance: policy, executor, approval, and the permission event
//! flow bound together.

use std::sync::Arc;
use std::time::{Duration, Instant};

use agentd_events::{EventLog, LogEntry};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout};
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
    /// Fails closed with [`SandboxError::Policy`] when the policy fails
    /// validation and with [`SandboxError::UnsupportedPlatform`] when the
    /// platform has no confinement layer (see
    /// [`ConfinedProcessExecutor`](crate::ConfinedProcessExecutor)).
    pub fn new(
        policy: &Policy,
        log: EventLog,
        agent_id: impl Into<String>,
    ) -> Result<Self, SandboxError> {
        policy.validate()?;
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
    /// Fails closed with [`SandboxError::Policy`] when the policy fails
    /// validation.
    pub fn with_executor(
        policy: &Policy,
        log: EventLog,
        agent_id: impl Into<String>,
        executor: Arc<dyn Executor>,
    ) -> Result<Self, SandboxError> {
        policy.validate()?;
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

    /// Records the decision and, when granted, runs the command to completion.
    ///
    /// A denial or timeout returns a denied [`ExecResult`] without running
    /// anything.
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

        if !self.authorize(&sandbox_id, &request_id, command).await? {
            return Ok(ExecResult::denied("the command was not approved"));
        }

        let started = Instant::now();
        let result = self.executor.exec(command).await;
        if let Some(violation) = violation::classify_violation(&result) {
            self.log
                .publish(violation::violation_event(
                    &sandbox_id,
                    &request_id,
                    &self.agent_id,
                    command,
                    &violation,
                ))
                .await?;
        }
        self.log
            .publish(events::exec_completed(
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

    /// Spawns `command` as a long-lived session with piped stdio.
    ///
    /// The session goes through the same approval as [`Sandbox::exec`] once, at
    /// spawn: `sandbox.session.started` is appended after the process starts,
    /// and [`SandboxedProcess::wait`] appends `sandbox.session.exited` with its
    /// terminal state. Output is not captured, so the caller owns the pipes.
    ///
    /// # Errors
    ///
    /// Returns [`SandboxError::Denied`] when an approver denies the spawn,
    /// [`SandboxError::Spawn`] when the process cannot be started (including an
    /// executor that cannot run sessions), and [`SandboxError::Publish`] when a
    /// lifecycle event cannot be durably appended.
    pub async fn spawn(
        &self,
        command: &str,
    ) -> Result<SandboxedProcess, SandboxError> {
        let sandbox_id = self.id.to_string();
        let request_id = Uuid::now_v7().to_string();

        if !self.authorize(&sandbox_id, &request_id, command).await? {
            return Err(SandboxError::Denied);
        }

        let child = self.executor.spawn(command).await?;
        let session_id = Uuid::now_v7();
        self.log
            .publish(events::session_started(
                &sandbox_id,
                &request_id,
                &self.agent_id,
                command,
                &session_id.to_string(),
            ))
            .await?;
        Ok(SandboxedProcess {
            id: session_id,
            pid: child.id(),
            child,
            log: self.log.clone(),
            sandbox_id,
            request_id,
            agent_id: self.agent_id.clone(),
            command: command.to_string(),
            started: Instant::now(),
            // The executor owns the profile and scratch the child runs under;
            // holding it keeps them alive for the session's lifetime.
            keepalive: Arc::clone(&self.executor),
        })
    }

    /// Runs the approval step, returning whether the command may run. A timeout
    /// records a denial itself; an approver's decision is already in the log.
    async fn authorize(
        &self,
        sandbox_id: &str,
        request_id: &str,
        command: &str,
    ) -> Result<bool, SandboxError> {
        match self.approval {
            Approval::Auto => {
                self.log
                    .publish(events::permission_requested(
                        sandbox_id,
                        request_id,
                        &self.agent_id,
                        command,
                        DECISION_AUTO,
                    ))
                    .await?;
                self.log
                    .publish(events::permission_granted(
                        sandbox_id,
                        request_id,
                        &self.agent_id,
                        command,
                    ))
                    .await?;
                Ok(true)
            },
            Approval::Required { timeout } => {
                // Subscribe before publishing, so the approver's decision
                // cannot be missed between the request and the wait.
                let mut decisions = self.log.subscribe();
                self.log
                    .publish(events::permission_requested(
                        sandbox_id,
                        request_id,
                        &self.agent_id,
                        command,
                        DECISION_PENDING,
                    ))
                    .await?;
                match await_decision(&mut decisions, request_id, timeout).await {
                    Decision::Granted => Ok(true),
                    Decision::Denied => Ok(false),
                    Decision::TimedOut => {
                        self.log
                            .publish(events::permission_denied(
                                sandbox_id,
                                request_id,
                                &self.agent_id,
                                command,
                            ))
                            .await?;
                        Ok(false)
                    },
                }
            },
        }
    }
}

/// A long-lived confined process with piped stdio.
///
/// Take the pipes with [`take_stdin`](SandboxedProcess::take_stdin),
/// [`take_stdout`](SandboxedProcess::take_stdout), and
/// [`take_stderr`](SandboxedProcess::take_stderr); the output is not captured
/// for you. The process is killed on drop if it is still running.
pub struct SandboxedProcess {
    id: Uuid,
    pid: Option<u32>,
    child: Child,
    log: EventLog,
    sandbox_id: String,
    request_id: String,
    agent_id: String,
    command: String,
    started: Instant,
    /// Keeps the executor (and its profile and scratch) alive for the child.
    #[expect(dead_code, reason = "the field is a keepalive, never read")]
    keepalive: Arc<dyn Executor>,
}

impl SandboxedProcess {
    /// The process's id, correlating the `sandbox.session.*` lifecycle events
    /// emitted for it (`data.session_id`).
    #[must_use]
    pub const fn id(&self) -> Uuid {
        self.id
    }

    /// Takes the child's standard input.
    pub const fn take_stdin(&mut self) -> Option<ChildStdin> {
        self.child.stdin.take()
    }

    /// Takes the child's standard output.
    pub const fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.child.stdout.take()
    }

    /// Takes the child's standard error.
    pub const fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.child.stderr.take()
    }

    /// Kills the process group and reaps the child.
    ///
    /// Reaping matters: a supervisor that cancels
    /// [`wait`](SandboxedProcess::wait) (for example on a lifetime timeout)
    /// would otherwise leave a zombie per spawned process.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the child cannot be signalled.
    pub async fn kill(&mut self) -> std::io::Result<()> {
        if let Some(pid) = self.pid
            && let Ok(raw) = i32::try_from(pid)
        {
            // The child leads its own process group (set at spawn), so the
            // whole tree dies, not just the shell.
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(raw),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
        let signalled = self.child.start_kill();
        let _ = self.child.wait().await;
        signalled
    }

    /// Waits for the process to exit, appends `sandbox.session.exited` (the
    /// sandbox's session-event namespace), and returns its exit code.
    ///
    /// # Errors
    ///
    /// Returns [`SandboxError::Io`] when waiting fails and
    /// [`SandboxError::Publish`] when the exit event cannot be appended.
    pub async fn wait(&mut self) -> Result<i32, SandboxError> {
        let status = self.child.wait().await?;
        let exit_code = status.code().unwrap_or_else(|| {
            use std::os::unix::process::ExitStatusExt as _;
            status
                .signal()
                .map_or(crate::process::EXIT_CANNOT_EXECUTE, |signal| 128 + signal)
        });
        self.log
            .publish(events::session_exited(
                &self.sandbox_id,
                &self.request_id,
                &self.agent_id,
                &self.command,
                &self.id.to_string(),
                exit_code,
                u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX),
            ))
            .await?;
        Ok(exit_code)
    }
}

impl std::fmt::Debug for SandboxedProcess {
    fn fmt(
        &self,
        formatter: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        formatter
            .debug_struct("SandboxedProcess")
            .field("id", &self.id)
            .field("command", &self.command)
            .finish_non_exhaustive()
    }
}

impl Drop for SandboxedProcess {
    fn drop(&mut self) {
        // Best-effort: a process dropped without `wait` must not leak a
        // running child.
        let _ = self.child.start_kill();
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
    use super::{Approval, Sandbox, SandboxedProcess};
    use crate::error::SandboxError;
    use crate::executor::{ExecResult, Executor, SpawnError};
    use crate::policy::{Access, FsEntry, FsPolicy, Limits, Policy};
    use agentd_events::{Event, EventLog, LogEntry};
    use async_trait::async_trait;
    use std::path::{Path, PathBuf};
    use std::process::Stdio;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

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

    /// An executor that runs commands unconfined through `/bin/sh`, so the
    /// session API and its event flow can be tested without Seatbelt.
    struct PlainExecutor;

    #[async_trait]
    impl Executor for PlainExecutor {
        async fn exec(
            &self,
            command: &str,
        ) -> ExecResult {
            let output = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(command)
                .output()
                .expect("sh should run");
            ExecResult {
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                exit_code: output.status.code().unwrap_or(1),
                denied: false,
            }
        }

        async fn spawn(
            &self,
            command: &str,
        ) -> Result<tokio::process::Child, SpawnError> {
            let child = tokio::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(command)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?;
            Ok(child)
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

    fn plain_sandbox() -> (Sandbox, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let sandbox = Sandbox::with_executor(
            &policy(),
            open_log(dir.path()),
            "coder-1",
            Arc::new(PlainExecutor),
        )
        .expect("valid policy");
        (sandbox, dir)
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
            assert_eq!(event.data["command"], "cargo test --workspace");
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
                    let command = event.data["command"].as_str().unwrap_or_default();
                    let reply = if decision == "grant" {
                        crate::events::permission_granted(sandbox_id, request_id, agent_id, command)
                    } else {
                        crate::events::permission_denied(sandbox_id, request_id, agent_id, command)
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

    #[tokio::test]
    async fn spawn_publishes_session_started_and_exited() {
        let (sandbox, _dir) = plain_sandbox();
        let mut subscriber = sandbox.log().subscribe();

        let mut session: SandboxedProcess =
            sandbox.spawn("exit 3").await.expect("spawn should succeed");
        let session_id = session.id().to_string();
        let exit_code = session.wait().await.expect("wait should succeed");

        assert_eq!(exit_code, 3);
        let events = drain(&mut subscriber);
        let types: Vec<&str> = events.iter().map(|event| event.r#type.as_str()).collect();
        assert_eq!(
            types,
            [
                crate::events::PERMISSION_REQUESTED,
                crate::events::PERMISSION_GRANTED,
                crate::events::SESSION_STARTED,
                crate::events::SESSION_EXITED,
            ]
        );
        assert_eq!(events[2].data["session_id"], session_id);
        assert_eq!(events[3].data["exit_code"], 3);
        assert_eq!(events[3].data["session_id"], session_id);
    }

    #[tokio::test]
    async fn a_session_streams_stdin_to_stdout() {
        let (sandbox, _dir) = plain_sandbox();

        let mut session = sandbox.spawn("cat").await.expect("spawn should succeed");
        let mut stdin = session.take_stdin().expect("stdin");
        let mut stdout = session.take_stdout().expect("stdout");

        stdin.write_all(b"hello\n").await.expect("write stdin");
        drop(stdin);

        let mut line = String::new();
        stdout.read_to_string(&mut line).await.expect("read stdout");
        assert_eq!(line, "hello\n");

        let exit_code = session.wait().await.expect("wait should succeed");
        assert_eq!(exit_code, 0);
    }

    #[tokio::test]
    async fn spawn_is_denied_by_an_approver() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let log = open_log(dir.path());
        let sandbox =
            Sandbox::with_executor(&policy(), log.clone(), "coder-1", Arc::new(PlainExecutor))
                .expect("valid policy")
                .with_approval(Approval::Required {
                    timeout: Duration::from_secs(5),
                });
        let mut subscriber = sandbox.log().subscribe();
        let approver = spawn_approver(&log, "deny");

        let error = sandbox
            .spawn("cat")
            .await
            .expect_err("a denied spawn must fail");

        assert!(matches!(error, SandboxError::Denied));
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
    async fn an_executor_without_sessions_fails_to_spawn() {
        let (sandbox, _executor, _dir) = sandbox(&policy());

        let error = sandbox
            .spawn("cat")
            .await
            .expect_err("a one-shot executor must refuse a session");

        assert!(matches!(error, SandboxError::Spawn(_)));
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
            Err(SandboxError::Policy(_))
        ));
    }
}
