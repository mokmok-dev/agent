//! The session: its identity, its state machine, and the registry that owns it.
//!
//! A session is one live agent process, one workspace, and one confinement
//! policy, with a lifetime the daemon owns. Its lifecycle is a state machine
//! with a terminal state (`docs/session/lifecycle.md`), not a set of flags, so
//! an invalid state cannot be represented and no caller can half-start a
//! session.
//!
//! The state is the only mutable thing a [`Session`] carries in this milestone.
//! The resources a session owns (a scratch directory, a confined process, an
//! egress allowlist) are added by the milestones that create them, so the
//! teardown order they need stays visible here when they arrive.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::path::{Path, PathBuf};

use crate::Error;

/// The stable identity of a session, derived from the bus subject.
///
/// The id a client uses is the id the log carries. It is validated because it
/// becomes part of a subject and, later, part of a path.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SessionId(String);

/// The longest session id accepted.
const MAX_SESSION_ID_LEN: usize = 128;

impl SessionId {
    /// Build an id from `value`, rejecting one that is empty, too long, or
    /// unsafe as a subject or path component.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidSessionId`] when `value` is empty, longer than
    /// 128 characters, contains anything but ASCII letters, digits, `.`, `_`,
    /// or `-`, or is `.` or `..`.
    pub fn new(value: impl Into<String>) -> Result<Self, Error> {
        let value = value.into();
        let safe = value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));
        if value.is_empty()
            || value.len() > MAX_SESSION_ID_LEN
            || !safe
            || value == "."
            || value == ".."
        {
            return Err(Error::InvalidSessionId(value));
        }
        Ok(Self(value))
    }

    /// The id as a string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The bus subject a session's own events are published under.
    #[must_use]
    pub fn subject(&self) -> String {
        format!("agent.session.{}", self.0)
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(
        &self,
        formatter: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Where a session is in its lifecycle.
///
/// Three states are terminal. A terminal session accepts no further
/// transition, so a consumer that waits for a session to start cannot hang on
/// one that never will, and a second stop of a stopped session is a no-op.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Accepted, with its resources being created.
    Starting,
    /// The agent process is alive.
    Running,
    /// A stop was requested and the process is being reaped.
    Stopping,
    /// The process was stopped on request.
    Stopped,
    /// The process ended on its own.
    Exited,
    /// The session could not start.
    Failed,
}

impl State {
    /// Whether no further transition is possible.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Stopped | Self::Exited | Self::Failed)
    }
}

/// One session: its identity, its state, and the workspace it holds.
#[derive(Debug)]
pub struct Session {
    id: SessionId,
    state: State,
    workspace: PathBuf,
}

impl Session {
    /// The session's id.
    #[must_use]
    pub const fn id(&self) -> &SessionId {
        &self.id
    }

    /// The session's current state.
    #[must_use]
    pub const fn state(&self) -> State {
        self.state
    }

    /// The workspace root the session acts on.
    #[must_use]
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// Move a session that finished starting to `Running`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidTransition`] unless the session is `Starting`.
    pub fn mark_running(&mut self) -> Result<(), Error> {
        self.transition("start", State::Starting, State::Running)
    }

    /// Move a session whose setup failed to `Failed`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidTransition`] unless the session is `Starting`.
    pub fn mark_failed(&mut self) -> Result<(), Error> {
        self.transition("fail", State::Starting, State::Failed)
    }

    /// Move a running session whose process ended on its own to `Exited`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidTransition`] unless the session is `Running`.
    pub fn mark_exited(&mut self) -> Result<(), Error> {
        self.transition("exit", State::Running, State::Exited)
    }

    /// Request that a running session stop, moving it to `Stopping`.
    ///
    /// A stop of a session that is no longer running is a no-op, so a client
    /// that stops a session twice does not reach a second process. This is the
    /// transition that sends the kill signal in a later milestone.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidTransition`] unless the session is `Running`.
    pub fn request_stop(&mut self) -> Result<(), Error> {
        self.transition("stop", State::Running, State::Stopping)
    }

    /// Move a stopping session whose process has been reaped to `Stopped`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidTransition`] unless the session is `Stopping`.
    pub fn mark_stopped(&mut self) -> Result<(), Error> {
        self.transition("confirm the stop of", State::Stopping, State::Stopped)
    }

    /// Apply one transition, requiring `from` and requiring the target not to be
    /// terminal, so a terminal state is never left.
    fn transition(
        &mut self,
        action: &'static str,
        from: State,
        to: State,
    ) -> Result<(), Error> {
        if self.state != from {
            return Err(Error::InvalidTransition {
                action,
                state: self.state,
            });
        }
        self.state = to;
        Ok(())
    }
}

/// Every session the daemon knows, and the workspace each one holds.
///
/// The registry is the single owner of session writes. A transition goes
/// through it so the workspace reservation stays in step with the state, and so
/// two callers (the event handler and the process reaper) cannot drive one
/// session's state at once. It holds no internal lock: the daemon serializes
/// access, which is the one place the lock belongs.
#[derive(Debug, Default)]
pub struct SessionRegistry {
    sessions: HashMap<SessionId, Session>,
    /// The workspace each live (non-terminal) session holds. A terminal session
    /// releases its root, so a client can reopen a session on the same workspace.
    reserved: HashMap<PathBuf, SessionId>,
}

impl SessionRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How many sessions the registry holds, terminal ones included.
    #[must_use]
    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    /// Whether the registry holds no sessions.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    /// Reserve `id` and `workspace`, starting a session.
    ///
    /// # Errors
    ///
    /// Returns [`Error::SessionExists`] when `id` is already live, and
    /// [`Error::WorkspaceBusy`] when another live session holds `workspace`. A
    /// terminal session's root is free, so reopening a workspace is allowed.
    pub fn insert(
        &mut self,
        id: SessionId,
        workspace: PathBuf,
    ) -> Result<&Session, Error> {
        let entry = match self.sessions.entry(id.clone()) {
            Entry::Occupied(_) => return Err(Error::SessionExists(id)),
            Entry::Vacant(slot) => slot,
        };
        if let Some(holder) = self.reserved.get(&workspace) {
            return Err(Error::WorkspaceBusy {
                workspace,
                session: holder.clone(),
            });
        }

        self.reserved.insert(workspace.clone(), id.clone());
        Ok(entry.insert(Session {
            id,
            state: State::Starting,
            workspace,
        }))
    }

    /// A session by id, or `None`.
    #[must_use]
    pub fn get(
        &self,
        id: &SessionId,
    ) -> Option<&Session> {
        self.sessions.get(id)
    }

    /// The live session holding `workspace`, or `None`.
    #[must_use]
    pub fn holder_of(
        &self,
        workspace: &Path,
    ) -> Option<&SessionId> {
        self.reserved.get(workspace)
    }

    /// Run `transition` on the session `id`, releasing the workspace when the
    /// session reaches a terminal state.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NoSuchSession`] for an unknown id, and the transition's
    /// own [`Error::InvalidTransition`] when the state does not allow it.
    pub fn apply<F>(
        &mut self,
        id: &SessionId,
        transition: F,
    ) -> Result<State, Error>
    where
        F: FnOnce(&mut Session) -> Result<(), Error>,
    {
        let session = self
            .sessions
            .get_mut(id)
            .ok_or_else(|| Error::NoSuchSession(id.clone()))?;
        transition(session)?;
        let state = session.state;
        if state.is_terminal() {
            let workspace = session.workspace.clone();
            self.reserved.remove(&workspace);
        }
        Ok(state)
    }

    /// Drop a session record entirely.
    ///
    /// A terminal session stays in the registry so its outcome is queryable;
    /// this removes it once a caller no longer needs it.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NoSuchSession`] for an unknown id.
    pub fn remove(
        &mut self,
        id: &SessionId,
    ) -> Result<Session, Error> {
        let session = self
            .sessions
            .remove(id)
            .ok_or_else(|| Error::NoSuchSession(id.clone()))?;
        self.reserved.remove(&session.workspace);
        Ok(session)
    }
}

#[cfg(test)]
mod tests {
    // Tests for the session core: id validation, the six-state machine, the
    // workspace reservation, and the registry's ownership of transitions. No
    // process is spawned, so these run on every host.

    use super::*;

    fn id(value: &str) -> SessionId {
        SessionId::new(value).expect("a valid id")
    }

    fn workspace(name: &str) -> PathBuf {
        PathBuf::from("/workspaces").join(name)
    }

    #[test]
    fn a_valid_id_is_accepted() {
        for value in ["s-1", "run_2", "a.b", &"x".repeat(128)] {
            assert!(SessionId::new(value).is_ok(), "`{value}` is valid");
        }
    }

    #[test]
    fn an_unsafe_id_is_rejected() {
        for value in ["", ".", "..", "a/b", "a\\b", "a b", "süß", &"x".repeat(129)] {
            assert!(
                matches!(SessionId::new(value), Err(Error::InvalidSessionId(_))),
                "`{value}` must be rejected"
            );
        }
    }

    #[test]
    fn a_subject_names_the_session() {
        assert_eq!(id("s-1").subject(), "agent.session.s-1");
    }

    #[test]
    fn an_id_reports_itself() {
        // The exact string, not a constant, so an `as_str` that returned a
        // placeholder is caught.
        assert_eq!(id("run_7").as_str(), "run_7");
    }

    #[test]
    fn a_registry_reports_whether_it_holds_anything() {
        // Both directions, so a constant `true` or `false` is caught.
        let mut registry = SessionRegistry::new();
        assert!(registry.is_empty());
        registry.insert(id("s"), workspace("w")).expect("inserts");
        assert!(!registry.is_empty());
    }

    #[test]
    fn only_the_last_three_states_are_terminal() {
        assert!(!State::Starting.is_terminal());
        assert!(!State::Running.is_terminal());
        assert!(!State::Stopping.is_terminal());
        assert!(State::Stopped.is_terminal());
        assert!(State::Exited.is_terminal());
        assert!(State::Failed.is_terminal());
    }

    #[test]
    fn a_session_starts_in_starting() {
        let id = id("s");
        let mut registry = SessionRegistry::new();
        let session = registry
            .insert(id.clone(), workspace("w"))
            .expect("inserts");
        assert_eq!(session.state(), State::Starting);
        assert_eq!(session.id(), &id);
    }

    #[test]
    fn a_started_session_runs() {
        let id = id("s");
        let mut registry = SessionRegistry::new();
        registry
            .insert(id.clone(), workspace("w"))
            .expect("inserts");

        let state = registry.apply(&id, Session::mark_running).expect("runs");
        assert_eq!(state, State::Running);
        assert_eq!(registry.get(&id).expect("present").state(), State::Running);
    }

    #[test]
    fn a_setup_error_fails_a_starting_session() {
        let id = id("s");
        let mut registry = SessionRegistry::new();
        registry
            .insert(id.clone(), workspace("w"))
            .expect("inserts");

        let state = registry.apply(&id, Session::mark_failed).expect("fails");
        assert_eq!(state, State::Failed);
    }

    #[test]
    fn a_failed_session_never_reports_running() {
        // The property the design states: a consumer waiting for `started`
        // cannot hang, and a failed setup cannot be started afterwards.
        let id = id("s");
        let mut registry = SessionRegistry::new();
        registry
            .insert(id.clone(), workspace("w"))
            .expect("inserts");
        registry.apply(&id, Session::mark_failed).expect("fails");

        assert!(matches!(
            registry.apply(&id, Session::mark_running),
            Err(Error::InvalidTransition {
                state: State::Failed,
                ..
            })
        ));
    }

    #[test]
    fn a_running_session_exits_on_its_own() {
        let id = id("s");
        let mut registry = SessionRegistry::new();
        registry
            .insert(id.clone(), workspace("w"))
            .expect("inserts");
        registry.apply(&id, Session::mark_running).expect("runs");

        let state = registry.apply(&id, Session::mark_exited).expect("exits");
        assert_eq!(state, State::Exited);
    }

    #[test]
    fn a_running_session_stops_through_stopping() {
        let id = id("s");
        let mut registry = SessionRegistry::new();
        registry
            .insert(id.clone(), workspace("w"))
            .expect("inserts");
        registry.apply(&id, Session::mark_running).expect("runs");

        assert_eq!(
            registry.apply(&id, Session::request_stop).expect("stops"),
            State::Stopping
        );
        assert_eq!(
            registry
                .apply(&id, Session::mark_stopped)
                .expect("confirms stop"),
            State::Stopped
        );
    }

    #[test]
    fn a_stop_of_a_terminal_session_is_refused() {
        // A client that stops a session twice must not reach a second process.
        let id = id("s");
        let mut registry = SessionRegistry::new();
        registry
            .insert(id.clone(), workspace("w"))
            .expect("inserts");
        registry.apply(&id, Session::mark_running).expect("runs");
        registry.apply(&id, Session::request_stop).expect("stops");
        registry
            .apply(&id, Session::mark_stopped)
            .expect("confirms stop");

        assert!(matches!(
            registry.apply(&id, Session::request_stop),
            Err(Error::InvalidTransition {
                state: State::Stopped,
                ..
            })
        ));
    }

    #[test]
    fn an_exited_session_cannot_be_stopped() {
        // The process already ended on its own; a stop must not try to kill it.
        let id = id("s");
        let mut registry = SessionRegistry::new();
        registry
            .insert(id.clone(), workspace("w"))
            .expect("inserts");
        registry.apply(&id, Session::mark_running).expect("runs");
        registry.apply(&id, Session::mark_exited).expect("exits");

        assert!(matches!(
            registry.apply(&id, Session::request_stop),
            Err(Error::InvalidTransition {
                state: State::Exited,
                ..
            })
        ));
    }

    #[test]
    fn a_starting_session_cannot_exit() {
        // Only a running process can end on its own.
        let id = id("s");
        let mut registry = SessionRegistry::new();
        registry
            .insert(id.clone(), workspace("w"))
            .expect("inserts");

        assert!(matches!(
            registry.apply(&id, Session::mark_exited),
            Err(Error::InvalidTransition {
                state: State::Starting,
                ..
            })
        ));
    }

    #[test]
    fn a_live_workspace_is_reserved() {
        let mut registry = SessionRegistry::new();
        let first = id("a");
        registry
            .insert(first.clone(), workspace("w"))
            .expect("inserts");

        assert_eq!(registry.holder_of(&workspace("w")), Some(&first));
        assert!(matches!(
            registry.insert(id("b"), workspace("w")),
            Err(Error::WorkspaceBusy { .. })
        ));
    }

    #[test]
    fn a_terminal_session_releases_its_workspace() {
        // A client reopens a session on the same workspace once the first ends.
        let mut registry = SessionRegistry::new();
        let first = id("a");
        registry
            .insert(first.clone(), workspace("w"))
            .expect("inserts");
        registry.apply(&first, Session::mark_failed).expect("fails");

        assert_eq!(registry.holder_of(&workspace("w")), None);
        registry
            .insert(id("b"), workspace("w"))
            .expect("the root is free again");
    }

    #[test]
    fn every_terminal_state_releases_the_workspace() {
        // All three terminal states release the root, not just `Failed`, so a
        // session that stopped or exited also frees its workspace.
        fn assert_released(
            registry: &SessionRegistry,
            reached: State,
        ) {
            assert!(reached.is_terminal(), "{reached:?} is terminal");
            assert_eq!(
                registry.holder_of(&workspace("w")),
                None,
                "{reached:?} releases the workspace"
            );
        }

        let mut stopped = SessionRegistry::new();
        let session = id("s");
        stopped
            .insert(session.clone(), workspace("w"))
            .expect("inserts");
        stopped
            .apply(&session, Session::mark_running)
            .expect("runs");
        stopped
            .apply(&session, Session::request_stop)
            .expect("stops");
        let reached = stopped
            .apply(&session, Session::mark_stopped)
            .expect("confirms");
        assert_released(&stopped, reached);

        let mut exited = SessionRegistry::new();
        let session = id("s");
        exited
            .insert(session.clone(), workspace("w"))
            .expect("inserts");
        exited.apply(&session, Session::mark_running).expect("runs");
        let reached = exited.apply(&session, Session::mark_exited).expect("exits");
        assert_released(&exited, reached);

        let mut failed = SessionRegistry::new();
        let session = id("s");
        failed
            .insert(session.clone(), workspace("w"))
            .expect("inserts");
        let reached = failed.apply(&session, Session::mark_failed).expect("fails");
        assert_released(&failed, reached);
    }

    #[test]
    fn a_duplicate_id_is_refused() {
        let mut registry = SessionRegistry::new();
        let taken = id("a");
        registry
            .insert(taken.clone(), workspace("w"))
            .expect("inserts");

        assert!(matches!(
            registry.insert(taken, workspace("other")),
            Err(Error::SessionExists(_))
        ));
    }

    #[test]
    fn distinct_workspaces_both_start() {
        let mut registry = SessionRegistry::new();
        let first = id("a");
        let second = id("b");
        registry
            .insert(first.clone(), workspace("w1"))
            .expect("inserts");
        registry
            .insert(second.clone(), workspace("w2"))
            .expect("inserts");

        assert_eq!(registry.len(), 2);
        assert_eq!(registry.holder_of(&workspace("w1")), Some(&first));
        assert_eq!(registry.holder_of(&workspace("w2")), Some(&second));
    }

    #[test]
    fn an_unknown_session_has_no_entry() {
        let mut registry = SessionRegistry::new();
        assert!(registry.get(&id("ghost")).is_none());
        assert!(matches!(
            registry.apply(&id("ghost"), Session::mark_running),
            Err(Error::NoSuchSession(_))
        ));
        assert!(matches!(
            registry.remove(&id("ghost")),
            Err(Error::NoSuchSession(_))
        ));
    }

    #[test]
    fn removing_a_live_session_frees_its_workspace() {
        let mut registry = SessionRegistry::new();
        let first = id("a");
        registry
            .insert(first.clone(), workspace("w"))
            .expect("inserts");

        let removed = registry.remove(&first).expect("removes");
        assert_eq!(removed.id(), &first);
        assert!(registry.is_empty());
        assert_eq!(registry.holder_of(&workspace("w")), None);
    }
}
