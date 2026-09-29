//! The daemon that runs one confined agent per session.
//!
//! It is the one crate that depends on both [`sandbox`] and [`agent`], and the
//! only place where the two meet. See `docs/session/` for its design.
//!
//! Milestone 2 is the **session core**: the [`SessionId`], the six-state
//! [`session::State`] machine, and the [`session::SessionRegistry`] that owns
//! the sessions. It spawns nothing and depends on neither the sandbox nor the
//! bus yet, so it is a pure data model and its behavior is tested as pure
//! transitions. The pieces a session will own (a scratch directory, a confined
//! process, an egress allowlist) arrive with the milestones that create them.

pub mod bus;
pub mod image;
pub mod launcher;
pub mod manager;
pub mod session;

pub use bus::{BusClient, BusPublisher};
pub use image::AgentImage;
pub use launcher::{LaunchRequest, LaunchedProcess, Launcher};
pub use manager::{Manager, ManagerConfig};
pub use session::{Session, SessionId, SessionRegistry, State};

/// Everything that can go wrong in a session operation.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A session id was empty or unsafe.
    #[error("invalid session id `{0}`")]
    InvalidSessionId(String),
    /// A session with this id is already live.
    #[error("session `{0}` already exists")]
    SessionExists(SessionId),
    /// No session has this id.
    #[error("no session with id `{0}`")]
    NoSuchSession(SessionId),
    /// A workspace root is already reserved by a live session.
    #[error("workspace `{workspace}` is reserved by a live session `{session}`")]
    WorkspaceBusy {
        /// The workspace root that is already reserved.
        workspace: std::path::PathBuf,
        /// The live session that holds it.
        session: SessionId,
    },
    /// A transition is not valid from the session's current state.
    #[error("cannot {action} a session in state {state:?}")]
    InvalidTransition {
        /// The action that was refused.
        action: &'static str,
        /// The state the session was in.
        state: State,
    },
    /// No confinement backend is available on this host, so no session opened.
    #[error("no confinement backend is available on this host")]
    NoBackend,
    /// The agent program lies inside the workspace it would act on.
    #[error("the agent program `{0}` is inside the session workspace")]
    ProgramInsideWorkspace(std::path::PathBuf),
    /// The scratch directory could not be created.
    #[error("scratch directory error: {0}")]
    Scratch(String),
    /// The launcher could not start or stop the confined process.
    #[error("launcher error: {0}")]
    Launcher(String),
    /// The composed policy is not valid.
    #[error(transparent)]
    Policy(#[from] sandbox::policy::InvalidPolicy),
    /// The session registry's lock was poisoned.
    #[error("the session registry lock is poisoned")]
    Poisoned,
    /// The bus connection failed.
    #[error(transparent)]
    Bus(#[from] crate::bus::Error),
}
