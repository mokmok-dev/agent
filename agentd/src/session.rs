//! The daemon-side session manager: it launches and supervises a sandboxed
//! node.
//!
//! The manager reacts to a `session.requested` event by starting the configured
//! node, supervises its lifetime, and reports the managed session's lifecycle as
//! `session.*` events.
//!
//! The manager runs the *configured* command, never a command carried in the
//! event: `session.requested` supplies only session parameters (above all a
//! `session_id`), so a client cannot turn it into arbitrary execution. The
//! sandbox confinement lifecycle (`sandbox.session.*`) and the daemon's managed
//! lifecycle (`session.*`) are separate namespaces.
//!
//! Supervision is opt-in: a session may be restarted up to a bounded number of
//! times when it exits non-zero, and may be given a maximum lifetime, after
//! which it is killed. A client can ask for the active sessions with a
//! `session.status.requested` event.
//!
//! The durable log is the source of truth for the active set across a daemon
//! restart: on startup the manager folds the recorded lifecycle and fails every
//! session the log left open, because a restarted daemon cannot re-adopt a
//! process it did not spawn. See [`SessionManager::run`].

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use agentd_events::{Event, EventLog, LogEntry, LogError, Seq};
use agentd_sandbox::{Policy, Sandbox, SandboxError};
use serde_json::{Value, json};
use thiserror::Error as ThisError;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::watch;

/// A client requests that the daemon start a managed session. Reserved to
/// daemon-authority publishers (see `docs/architecture.md`).
pub const SESSION_REQUESTED: &str = "session.requested";
/// A client requests the list of active managed sessions.
pub const SESSION_STATUS_REQUESTED: &str = "session.status.requested";
/// The managed session's sandboxed process started.
pub const SESSION_STARTED: &str = "session.started";
/// The managed session's process restarted after a non-zero exit.
pub const SESSION_RESTARTED: &str = "session.restarted";
/// The managed session's process exited.
pub const SESSION_EXITED: &str = "session.exited";
/// The managed session could not be started, supervised, or was stopped.
pub const SESSION_FAILED: &str = "session.failed";
/// The response to a `session.status.requested`.
pub const SESSION_STATUS: &str = "session.status";

/// Errors returned while building or running the session manager.
#[derive(Debug, ThisError)]
pub enum SessionError {
    /// The manager's sandbox could not be built from the policy.
    #[error("the session sandbox could not be built: {0}")]
    Sandbox(#[from] SandboxError),
    /// The manager could not read the durable log to reconcile its state.
    #[error("the session log could not be read: {0}")]
    Log(#[from] LogError),
}

/// How the manager supervises a session's process.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Supervision {
    /// How many times to restart a session that exits non-zero before giving
    /// up. `0` never restarts (the default).
    pub max_restarts: u32,
    /// The delay before a restart.
    pub restart_backoff: Duration,
    /// A maximum lifetime; a session that outlives it is killed. `None` lets
    /// the session run until it exits (the default).
    pub lifetime: Option<Duration>,
}

impl Default for Supervision {
    fn default() -> Self {
        Self {
            max_restarts: 0,
            restart_backoff: Duration::from_secs(1),
            lifetime: None,
        }
    }
}

/// Launches and supervises a sandboxed node per `session.requested` event.
#[derive(Clone)]
pub struct SessionManager {
    log: EventLog,
    sandbox: Arc<Sandbox>,
    command: String,
    agent_id: String,
    supervision: Supervision,
    /// The active sessions, by id, with the restart count of each.
    ///
    /// A `std` mutex is sufficient because no critical section awaits; keeping
    /// it off the async mutex makes that invariant compiler-enforced.
    active: Arc<Mutex<BTreeMap<String, u32>>>,
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
            supervision: Supervision::default(),
            active: Arc::new(Mutex::new(BTreeMap::new())),
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
            supervision: Supervision::default(),
            active: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    /// Sets the supervision policy.
    #[must_use]
    pub const fn with_supervision(
        mut self,
        supervision: Supervision,
    ) -> Self {
        self.supervision = supervision;
        self
    }

    /// Runs until `shutdown` becomes `true`, launching a session for each
    /// `session.requested` event and answering `session.status.requested`.
    ///
    /// The in-memory active set is reconciled with the durable log first, so a
    /// restarted daemon learns about the sessions its predecessor started. That
    /// reconciliation reads the log once, synchronously, before the loop starts.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::Log`] when the log cannot be read to reconcile.
    pub async fn run(
        &self,
        mut shutdown: watch::Receiver<bool>,
    ) -> Result<(), SessionError> {
        // Subscribe before taking the snapshot so no event can slip between the
        // replay and the live stream; entries at or below the snapshot are the
        // ones reconciliation already folded and are skipped below.
        let mut events = self.log.subscribe();
        let snapshot = self.log.tail_seq();
        self.reconcile(snapshot).await?;
        loop {
            tokio::select! {
                _ = shutdown.changed() => return Ok(()),
                recorded = events.recv() => match recorded {
                    Ok(entry) if entry.seq <= snapshot => {},
                    Ok(entry) => match entry.event.r#type.as_str() {
                        SESSION_REQUESTED => self.launch(&entry.event),
                        SESSION_STATUS_REQUESTED => self.report_status().await,
                        _ => {},
                    },
                    Err(RecvError::Lagged(_)) => {},
                    Err(RecvError::Closed) => return Ok(()),
                },
            }
        }
    }

    /// Reconciles the active set with the durable log up to `through`.
    ///
    /// Every session the log shows as started but never terminated was running
    /// under the previous daemon and cannot be re-adopted (no process handle
    /// survives the restart), so each is recorded as failed with a reason. Any
    /// other session is left untouched, and the log is not re-launched from:
    /// folding `session.requested` never starts a session.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::Log`] when the log cannot be read.
    async fn reconcile(
        &self,
        through: Seq,
    ) -> Result<(), SessionError> {
        let reader = self.log.read_from(1)?;
        let interrupted = tokio::task::spawn_blocking(move || active_after(reader, through))
            .await
            .map_err(|error| SessionError::Log(LogError::Io(std::io::Error::other(error))))??;
        for session_id in interrupted.keys() {
            let event = session_failed(
                session_id,
                &self.agent_id,
                "the daemon restarted while the session was active",
            );
            if let Err(error) = self.log.publish(event).await {
                tracing::error!(%error, "failed to record an interrupted session");
            }
        }
        Ok(())
    }

    /// Borrows the active-session map.
    ///
    /// A poisoned lock is recovered: the map is a plain counter, so a panic
    /// while holding it cannot have left it inconsistent.
    fn active(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, u32>> {
        self.active.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Starts one supervised session for `request`.
    ///
    /// Returns once the supervision task is spawned, so the run loop is not
    /// blocked by a long-lived node.
    pub fn launch(
        &self,
        request: &Event,
    ) {
        let session_id = request
            .data
            .get("session_id")
            .and_then(Value::as_str)
            .unwrap_or(&request.id)
            .to_string();
        let manager = self.clone();
        tokio::spawn(async move { manager.supervise(session_id).await });
    }

    /// Publishes the active sessions in response to `session.status.requested`.
    async fn report_status(&self) {
        let sessions: Vec<Value> = {
            let active = self.active();
            active
                .iter()
                .map(|(session_id, restarts)| {
                    json!({ "session_id": session_id, "restarts": restarts })
                })
                .collect()
        };
        let event = session_status(&sessions);
        if let Err(error) = self.log.publish(event).await {
            tracing::error!(%error, "failed to record session status");
        }
    }

    /// Runs the session, restarting it within the supervision budget and
    /// killing it past its lifetime, then reports the terminal state.
    async fn supervise(
        &self,
        session_id: String,
    ) {
        let mut restarts = 0_u32;
        loop {
            self.active().insert(session_id.clone(), restarts);
            let announcement = if restarts == 0 {
                session_started(&session_id, &self.agent_id)
            } else {
                session_restarted(&session_id, &self.agent_id, restarts)
            };
            if let Err(error) = self.log.publish(announcement).await {
                tracing::error!(%error, "failed to record session start");
                self.active().remove(&session_id);
                return;
            }

            let started = Instant::now();
            let session = match self.sandbox.spawn(&self.command).await {
                Ok(session) => session,
                Err(error) => {
                    let event = session_failed(&session_id, &self.agent_id, &error.to_string());
                    let _ = self.log.publish(event).await;
                    self.active().remove(&session_id);
                    return;
                },
            };
            let outcome = self.wait_for(session).await;

            let restart = matches!(
                &outcome,
                Outcome::Exited(Ok(code)) if *code != 0 && restarts < self.supervision.max_restarts
            );
            if restart {
                restarts += 1;
                tokio::time::sleep(self.supervision.restart_backoff).await;
                continue;
            }

            self.active().remove(&session_id);
            let duration = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
            let event = match outcome {
                Outcome::Exited(Ok(exit_code)) => {
                    session_exited(&session_id, &self.agent_id, exit_code, duration)
                },
                Outcome::Exited(Err(error)) => {
                    session_failed(&session_id, &self.agent_id, &error.to_string())
                },
                Outcome::Lifetime => session_failed(
                    &session_id,
                    &self.agent_id,
                    "the session exceeded its maximum lifetime",
                ),
            };
            if let Err(error) = self.log.publish(event).await {
                tracing::error!(%error, "failed to record session terminal state");
            }
            return;
        }
    }

    /// Waits for the session to exit, or kills it when it outlives its
    /// configured lifetime.
    async fn wait_for(
        &self,
        mut session: agentd_sandbox::SandboxedProcess,
    ) -> Outcome {
        let Some(lifetime) = self.supervision.lifetime else {
            return Outcome::Exited(session.wait().await);
        };
        if let Ok(result) = tokio::time::timeout(lifetime, session.wait()).await {
            return Outcome::Exited(result);
        }
        if let Err(error) = session.kill().await {
            tracing::warn!(%error, "failed to kill an expired session");
        }
        Outcome::Lifetime
    }
}

/// How a supervised session ended.
enum Outcome {
    /// The process exited; `Ok` carries its code, `Err` a supervision error.
    Exited(Result<i32, SandboxError>),
    /// The session exceeded its lifetime and was killed.
    Lifetime,
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

/// Builds a `session.restarted` event.
#[must_use]
pub fn session_restarted(
    session_id: &str,
    agent_id: &str,
    restarts: u32,
) -> Event {
    let mut data = session_data(session_id, agent_id);
    data["restarts"] = json!(restarts);
    Event::new(SESSION_RESTARTED, data)
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

/// Builds a `session.status` event carrying the active sessions.
#[must_use]
pub fn session_status(sessions: &[Value]) -> Event {
    Event::new(SESSION_STATUS, json!({ "sessions": sessions }))
}

/// Folds the session lifecycle events up to `through` into the set of sessions
/// the log has not terminated: each surviving id with its recorded restart
/// count.
///
/// Only `session.started`, `session.restarted`, `session.exited`, and
/// `session.failed` affect the set; `session.requested` and the status events
/// are ignored, so replaying history never launches anything.
fn active_after(
    entries: impl Iterator<Item = Result<LogEntry, LogError>>,
    through: Seq,
) -> Result<BTreeMap<String, u32>, LogError> {
    let mut active: BTreeMap<String, u32> = BTreeMap::new();
    for entry in entries {
        let entry = entry?;
        if entry.seq > through {
            break;
        }
        let Some(session_id) = entry.event.data.get("session_id").and_then(Value::as_str) else {
            continue;
        };
        match entry.event.r#type.as_str() {
            SESSION_STARTED => {
                active.insert(session_id.to_string(), 0);
            },
            SESSION_RESTARTED => {
                let restarts = entry
                    .event
                    .data
                    .get("restarts")
                    .and_then(Value::as_u64)
                    .and_then(|count| u32::try_from(count).ok())
                    .unwrap_or(1);
                active.insert(session_id.to_string(), restarts);
            },
            SESSION_EXITED | SESSION_FAILED => {
                active.remove(session_id);
            },
            _ => {},
        }
    }
    Ok(active)
}

#[cfg(test)]
mod tests {
    use super::{
        SESSION_FAILED, SESSION_REQUESTED, SESSION_RESTARTED, SESSION_STARTED,
        SESSION_STATUS_REQUESTED, SessionManager, Supervision,
    };
    use agentd_events::{Event, EventLog, LogEntry};
    use agentd_sandbox::{
        Access, ExecResult, Executor, FsEntry, FsPolicy, Policy, Sandbox, ShellPolicy, SpawnError,
    };
    use async_trait::async_trait;
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
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
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .process_group(0)
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
        supervision: Supervision,
    ) -> SessionManager {
        let sandbox = Arc::new(
            Sandbox::with_executor(&policy(), log.clone(), "agent", executor).expect("sandbox"),
        );
        SessionManager::with_sandbox(log, sandbox, command, "agent").with_supervision(supervision)
    }

    fn request(session_id: &str) -> Event {
        Event::new(SESSION_REQUESTED, json!({ "session_id": session_id }))
    }

    /// Waits until a `type` event arrives, failing the test on timeout.
    async fn wait_for(
        receiver: &mut tokio::sync::broadcast::Receiver<LogEntry>,
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
        let manager = manager(
            log,
            Arc::new(PlainExecutor),
            "exit 7",
            Supervision::default(),
        );

        manager.launch(&request("s1"));

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
        let manager = manager(
            log,
            Arc::new(OneShotExecutor),
            "cat",
            Supervision::default(),
        );

        manager.launch(&request("s2"));

        let failed = wait_for(&mut subscriber, SESSION_FAILED).await;
        assert_eq!(failed.data["session_id"], "s2");
        assert!(failed.data["error"].as_str().is_some());
    }

    #[tokio::test]
    async fn a_non_zero_exit_is_restarted_within_the_budget() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = open_log(dir.path());
        let mut subscriber = log.subscribe();
        let supervision = Supervision {
            max_restarts: 1,
            restart_backoff: Duration::from_millis(10),
            lifetime: None,
        };
        let manager = manager(log, Arc::new(PlainExecutor), "exit 1", supervision);

        manager.launch(&request("s3"));

        let started = wait_for(&mut subscriber, SESSION_STARTED).await;
        assert_eq!(started.data["session_id"], "s3");
        let restarted = wait_for(&mut subscriber, SESSION_RESTARTED).await;
        assert_eq!(restarted.data["restarts"], 1);
        // The budget is exhausted after one restart, so the second non-zero
        // exit is terminal.
        let exited = wait_for(&mut subscriber, super::SESSION_EXITED).await;
        assert_eq!(exited.data["exit_code"], 1);
    }

    #[tokio::test]
    async fn a_session_past_its_lifetime_is_killed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = open_log(dir.path());
        let mut subscriber = log.subscribe();
        let supervision = Supervision {
            lifetime: Some(Duration::from_millis(100)),
            ..Supervision::default()
        };
        let manager = manager(log, Arc::new(PlainExecutor), "sleep 30", supervision);

        manager.launch(&request("s4"));

        let failed = wait_for(&mut subscriber, SESSION_FAILED).await;
        assert_eq!(failed.data["session_id"], "s4");
        assert!(
            failed.data["error"]
                .as_str()
                .is_some_and(|error| error.contains("lifetime"))
        );
    }

    #[tokio::test]
    async fn run_answers_a_status_request_with_the_active_sessions() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = open_log(dir.path());
        let supervision = Supervision {
            lifetime: Some(Duration::from_millis(500)),
            ..Supervision::default()
        };
        let manager = manager(
            log.clone(),
            Arc::new(PlainExecutor),
            "sleep 30",
            supervision,
        );
        let (sender, receiver) = watch::channel(false);
        let handle = tokio::spawn(async move { manager.run(receiver).await });

        // Give the run loop a moment to subscribe, then request a session.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let mut subscriber = log.subscribe();
        log.publish(request("s5")).await.expect("publish");
        wait_for(&mut subscriber, SESSION_STARTED).await;

        log.publish(Event::new(SESSION_STATUS_REQUESTED, json!({})))
            .await
            .expect("publish");
        let status = wait_for(&mut subscriber, super::SESSION_STATUS).await;
        let sessions = status.data["sessions"].as_array().expect("an array");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0]["session_id"], "s5");

        sender.send(true).expect("shutdown");
        handle
            .await
            .expect("join")
            .expect("run should stop cleanly");
    }

    #[test]
    fn active_after_folds_lifecycle_in_order() {
        let entries = vec![
            // A request is not a lifecycle event: replaying it must not start.
            Ok(LogEntry::new(1, request("s1"))),
            Ok(LogEntry::new(2, super::session_started("s1", "agent"))),
            Ok(LogEntry::new(3, super::session_restarted("s1", "agent", 2))),
            Ok(LogEntry::new(4, super::session_started("s2", "agent"))),
            Ok(LogEntry::new(5, super::session_exited("s2", "agent", 0, 5))),
            Ok(LogEntry::new(6, super::session_started("s3", "agent"))),
            Ok(LogEntry::new(
                7,
                super::session_failed("s3", "agent", "boom"),
            )),
        ];

        let active = super::active_after(entries.into_iter(), 7).expect("fold");

        assert_eq!(active, BTreeMap::from([(String::from("s1"), 2)]));
    }

    #[test]
    fn active_after_stops_at_the_snapshot() {
        let entries = vec![
            Ok(LogEntry::new(1, super::session_started("s1", "agent"))),
            Ok(LogEntry::new(2, super::session_exited("s1", "agent", 0, 1))),
        ];

        // The exit at seq 2 is beyond the snapshot, so it is not yet folded and
        // the session still looks active.
        let active = super::active_after(entries.into_iter(), 1).expect("fold");

        assert_eq!(active, BTreeMap::from([(String::from("s1"), 0)]));
    }

    #[tokio::test]
    async fn run_fails_a_session_left_open_by_a_previous_daemon() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = open_log(dir.path());
        log.publish(super::session_started("s6", "agent"))
            .await
            .expect("publish");
        let manager = manager(
            log.clone(),
            Arc::new(PlainExecutor),
            "exit 7",
            Supervision::default(),
        );
        let mut subscriber = log.subscribe();
        let (sender, receiver) = watch::channel(false);
        let handle = tokio::spawn(async move { manager.run(receiver).await });

        let failed = wait_for(&mut subscriber, SESSION_FAILED).await;
        assert_eq!(failed.data["session_id"], "s6");
        assert!(
            failed.data["error"]
                .as_str()
                .is_some_and(|error| error.contains("restarted")),
            "the failure must explain the restart: {:?}",
            failed.data["error"]
        );

        log.publish(Event::new(SESSION_STATUS_REQUESTED, json!({})))
            .await
            .expect("publish");
        let status = wait_for(&mut subscriber, super::SESSION_STATUS).await;
        assert!(
            status.data["sessions"]
                .as_array()
                .expect("an array")
                .is_empty(),
            "an interrupted session must not stay active"
        );

        sender.send(true).expect("shutdown");
        handle
            .await
            .expect("join")
            .expect("run should stop cleanly");
    }

    #[tokio::test]
    async fn run_leaves_a_terminated_session_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = open_log(dir.path());
        log.publish(super::session_started("s7", "agent"))
            .await
            .expect("publish");
        log.publish(super::session_exited("s7", "agent", 0, 3))
            .await
            .expect("publish");
        let manager = manager(
            log.clone(),
            Arc::new(PlainExecutor),
            "exit 0",
            Supervision::default(),
        );
        let mut subscriber = log.subscribe();
        let (sender, receiver) = watch::channel(false);
        let handle = tokio::spawn(async move { manager.run(receiver).await });

        log.publish(Event::new(SESSION_STATUS_REQUESTED, json!({})))
            .await
            .expect("publish");
        let status = wait_for(&mut subscriber, super::SESSION_STATUS).await;
        assert!(
            status.data["sessions"]
                .as_array()
                .expect("an array")
                .is_empty()
        );

        // Reconciliation runs before the status is answered, so nothing more
        // may be recorded for a session the log already terminated.
        let quiet = tokio::time::timeout(Duration::from_millis(100), subscriber.recv()).await;
        assert!(
            quiet.is_err(),
            "no event should follow the status: {quiet:?}"
        );

        sender.send(true).expect("shutdown");
        handle
            .await
            .expect("join")
            .expect("run should stop cleanly");
    }
}
