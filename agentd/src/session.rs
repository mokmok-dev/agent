//! The daemon-side session manager: it launches a sandboxed node when a
//! `session.requested` event appears on the log and reports the managed
//! session's lifecycle as `session.*` events.
//!
//! The manager runs the *configured* command, never a command carried in the
//! event: `session.requested` supplies only session parameters (above all a
//! `session_id`), so a client cannot turn it into arbitrary execution. The
//! sandbox confinement lifecycle (`sandbox.session.*`) and the daemon's managed
//! lifecycle (`session.*`) are separate namespaces.

use std::sync::Arc;
use std::time::Instant;

use agentd_events::{Event, EventLog};
use agentd_sandbox::{Policy, Sandbox, SandboxError};
use serde_json::{Value, json};
use thiserror::Error as ThisError;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::watch;

/// A client requests that the daemon start a managed session. Reserved to
/// daemon-authority publishers (see `docs/architecture.md`).
pub const SESSION_REQUESTED: &str = "session.requested";
/// The managed session's sandboxed process started.
pub const SESSION_STARTED: &str = "session.started";
/// The managed session's process exited.
pub const SESSION_EXITED: &str = "session.exited";
/// The managed session could not be started or supervised.
pub const SESSION_FAILED: &str = "session.failed";

/// Errors returned while building or running the session manager.
#[derive(Debug, ThisError)]
pub enum SessionError {
    /// The manager's sandbox could not be built from the policy.
    #[error("the session sandbox could not be built: {0}")]
    Sandbox(#[from] SandboxError),
}

/// Launches a sandboxed node per `session.requested` event.
#[derive(Clone)]
pub struct SessionManager {
    log: EventLog,
    sandbox: Arc<Sandbox>,
    command: String,
    agent_id: String,
}

impl SessionManager {
    /// Creates a manager that runs `command` in a sandbox built from `policy`.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::Sandbox`] when the policy is unusable or the
    /// platform has no confinement layer.
    pub fn new(
        log: EventLog,
        policy: &Policy,
        command: impl Into<String>,
        agent_id: impl Into<String>,
    ) -> Result<Self, SessionError> {
        let agent_id = agent_id.into();
        let sandbox = Arc::new(Sandbox::new(policy, log.clone(), agent_id.clone())?);
        Ok(Self {
            log,
            sandbox,
            command: command.into(),
            agent_id,
        })
    }

    /// Creates a manager around an existing sandbox, e.g. one with a fake
    /// executor in tests.
    #[must_use]
    pub fn with_sandbox(
        log: EventLog,
        sandbox: Arc<Sandbox>,
        command: impl Into<String>,
        agent_id: impl Into<String>,
    ) -> Self {
        Self {
            log,
            sandbox,
            command: command.into(),
            agent_id: agent_id.into(),
        }
    }

    /// Runs until `shutdown` becomes `true`, launching a session for each
    /// `session.requested` event.
    ///
    /// # Errors
    ///
    /// Never fails today; the signature leaves room for a fatal supervision
    /// error.
    pub async fn run(
        &self,
        mut shutdown: watch::Receiver<bool>,
    ) -> Result<(), SessionError> {
        let mut events = self.log.subscribe();
        loop {
            tokio::select! {
                _ = shutdown.changed() => return Ok(()),
                recorded = events.recv() => match recorded {
                    Ok(entry) => {
                        if entry.event.r#type == SESSION_REQUESTED {
                            self.launch(&entry.event).await;
                        }
                    },
                    Err(RecvError::Lagged(_)) => {},
                    Err(RecvError::Closed) => return Ok(()),
                },
            }
        }
    }

    /// Starts one session for `request` and reports its lifecycle.
    ///
    /// The spawn is awaited, but supervision runs in a background task so the
    /// run loop is not blocked by a long-lived node.
    pub async fn launch(
        &self,
        request: &Event,
    ) {
        let session_id = request
            .data
            .get("session_id")
            .and_then(Value::as_str)
            .unwrap_or(&request.id)
            .to_string();
        let started = Instant::now();

        match self.sandbox.spawn(&self.command).await {
            Ok(mut session) => {
                if let Err(error) = self
                    .log
                    .publish(session_started(&session_id, &self.agent_id))
                    .await
                {
                    tracing::error!(%error, "failed to record session start");
                    return;
                }
                let log = self.log.clone();
                let agent_id = self.agent_id.clone();
                tokio::spawn(async move {
                    match session.wait().await {
                        Ok(exit_code) => {
                            let duration =
                                u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                            let event = session_exited(&session_id, &agent_id, exit_code, duration);
                            if let Err(error) = log.publish(event).await {
                                tracing::error!(%error, "failed to record session exit");
                            }
                        },
                        Err(error) => {
                            let event = session_failed(&session_id, &agent_id, &error.to_string());
                            if let Err(error) = log.publish(event).await {
                                tracing::error!(%error, "failed to record session failure");
                            }
                        },
                    }
                });
            },
            Err(error) => {
                let event = session_failed(&session_id, &self.agent_id, &error.to_string());
                if let Err(error) = self.log.publish(event).await {
                    tracing::error!(%error, "failed to record session failure");
                }
            },
        }
    }
}

/// The fields every `session.*` event carries.
fn session_data(
    session_id: &str,
    agent_id: &str,
) -> Value {
    json!({ "session_id": session_id, "agent_id": agent_id })
}

/// Builds a `session.started` event.
#[must_use]
pub fn session_started(
    session_id: &str,
    agent_id: &str,
) -> Event {
    Event::new(SESSION_STARTED, session_data(session_id, agent_id))
}

/// Builds a `session.exited` event.
#[must_use]
pub fn session_exited(
    session_id: &str,
    agent_id: &str,
    exit_code: i32,
    duration_ms: u64,
) -> Event {
    let mut data = session_data(session_id, agent_id);
    data["exit_code"] = json!(exit_code);
    data["duration_ms"] = json!(duration_ms);
    Event::new(SESSION_EXITED, data)
}

/// Builds a `session.failed` event.
#[must_use]
pub fn session_failed(
    session_id: &str,
    agent_id: &str,
    error: &str,
) -> Event {
    let mut data = session_data(session_id, agent_id);
    data["error"] = json!(error);
    Event::new(SESSION_FAILED, data)
}

#[cfg(test)]
mod tests {
    use super::{SESSION_FAILED, SESSION_REQUESTED, SESSION_STARTED, SessionManager};
    use agentd_events::{Event, EventLog};
    use agentd_sandbox::{
        Access, ExecResult, Executor, FsEntry, FsPolicy, Policy, Sandbox, ShellPolicy, SpawnError,
    };
    use async_trait::async_trait;
    use serde_json::json;
    use std::path::{Path, PathBuf};
    use std::process::Stdio;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::watch;

    /// An executor that runs commands unconfined through `/bin/sh` so the
    /// manager can be tested without Seatbelt.
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

    /// An executor that cannot spawn sessions.
    struct OneShotExecutor;

    #[async_trait]
    impl Executor for OneShotExecutor {
        async fn exec(
            &self,
            _command: &str,
        ) -> ExecResult {
            ExecResult {
                stdout: String::new(),
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
            shell: ShellPolicy::default(),
            ..Policy::default()
        }
    }

    fn open_log(dir: &Path) -> EventLog {
        EventLog::open(dir.join("events.jsonl")).expect("log should open")
    }

    fn manager(
        log: EventLog,
        executor: Arc<dyn Executor>,
        command: &str,
    ) -> SessionManager {
        let sandbox = Arc::new(
            Sandbox::with_executor(&policy(), log.clone(), "agent", executor).expect("sandbox"),
        );
        SessionManager::with_sandbox(log, sandbox, command, "agent")
    }

    fn request(session_id: &str) -> Event {
        Event::new(SESSION_REQUESTED, json!({ "session_id": session_id }))
    }

    /// Waits until a `type` event arrives, failing the test on timeout.
    async fn wait_for(
        receiver: &mut tokio::sync::broadcast::Receiver<agentd_events::LogEntry>,
        r#type: &str,
    ) -> Event {
        let kind = r#type;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(remaining, receiver.recv()).await {
                Ok(Ok(recorded)) if recorded.event.r#type == kind => return recorded.event,
                Ok(Ok(_)) => {},
                Ok(Err(error)) => panic!("the log closed before {kind}: {error}"),
                Err(error) => panic!("timed out waiting for {kind}: {error}"),
            }
        }
    }

    #[tokio::test]
    async fn launch_reports_a_session_lifecycle() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = open_log(dir.path());
        let mut subscriber = log.subscribe();
        let manager = manager(log, Arc::new(PlainExecutor), "exit 7");

        manager.launch(&request("s1")).await;

        let started = wait_for(&mut subscriber, SESSION_STARTED).await;
        assert_eq!(started.data["session_id"], "s1");
        let exited = wait_for(&mut subscriber, super::SESSION_EXITED).await;
        assert_eq!(exited.data["session_id"], "s1");
        assert_eq!(exited.data["exit_code"], 7);
        assert!(exited.data["duration_ms"].as_u64().is_some());
    }

    #[tokio::test]
    async fn launch_reports_a_failed_session_when_spawning_is_unsupported() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = open_log(dir.path());
        let mut subscriber = log.subscribe();
        let manager = manager(log, Arc::new(OneShotExecutor), "cat");

        manager.launch(&request("s2")).await;

        let failed = wait_for(&mut subscriber, SESSION_FAILED).await;
        assert_eq!(failed.data["session_id"], "s2");
        assert!(failed.data["error"].as_str().is_some());
    }

    #[tokio::test]
    async fn run_launches_a_session_for_a_requested_event() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = open_log(dir.path());
        let manager = manager(log.clone(), Arc::new(PlainExecutor), "exit 0");
        let (sender, receiver) = watch::channel(false);
        let handle = tokio::spawn(async move { manager.run(receiver).await });

        // Give the run loop a moment to subscribe, then request a session.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let mut subscriber = log.subscribe();
        log.publish(request("s3")).await.expect("publish");

        let started = wait_for(&mut subscriber, SESSION_STARTED).await;
        assert_eq!(started.data["session_id"], "s3");

        sender.send(true).expect("shutdown");
        handle
            .await
            .expect("join")
            .expect("run should stop cleanly");
    }
}
