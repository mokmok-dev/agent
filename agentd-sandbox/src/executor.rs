//! The executor abstraction: the seam between the sandbox and whatever runs
//! the commands.
//!
//! There is exactly one production implementation (the layer-1 confined-process
//! executor); the trait exists so tests can inject a fake without spawning a
//! process. The policy is bound at construction, not per call, so a running
//! executor cannot widen its own permissions.

use async_trait::async_trait;
use thiserror::Error as ThisError;

/// Errors returned while spawning a long-lived session.
#[derive(Debug, ThisError)]
pub enum SpawnError {
    /// The process could not be started.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// The executor cannot run a long-lived session.
    #[error("the executor does not support long-lived sessions: {0}")]
    Unsupported(&'static str),
}

/// The outcome of running a command under the policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecResult {
    /// Captured standard output.
    pub stdout: String,
    /// Captured standard error.
    pub stderr: String,
    /// The process exit code.
    ///
    /// Conventions the built-in executor follows: `124` marks the wall-clock
    /// timeout, `126` marks a command that could not be executed (the stderr
    /// field says which), and `128 + signal` marks a killed process (a
    /// timeout or output-cap kill lands on `137`).
    pub exit_code: i32,
    /// Set when an approver denied the command before it ran.
    pub denied: bool,
}

impl ExecResult {
    /// A denial result: the approver refused the command, so nothing ran.
    #[must_use]
    pub fn denied(message: &str) -> Self {
        Self {
            stdout: String::new(),
            stderr: format!("[agentd-sandbox] {message}"),
            exit_code: 126,
            denied: true,
        }
    }

    /// Whether an approver denied the command.
    #[must_use]
    pub const fn is_denied(&self) -> bool {
        self.denied
    }
}

/// Runs a command string under the policy bound at construction.
#[async_trait]
pub trait Executor: Send + Sync {
    /// Runs `command` and captures its output.
    async fn exec(
        &self,
        command: &str,
    ) -> ExecResult;

    /// Spawns `command` as a long-lived process with piped stdio, without
    /// waiting for it to exit.
    ///
    /// The default fails: an executor that only supports one-shot commands
    /// cannot back a session. The layer-1 confined-process executor overrides
    /// it.
    ///
    /// # Errors
    ///
    /// Returns [`SpawnError::Unsupported`] by default, or
    /// [`SpawnError::Io`] when the process cannot be started.
    async fn spawn(
        &self,
        _command: &str,
    ) -> Result<tokio::process::Child, SpawnError> {
        Err(SpawnError::Unsupported("this executor is one-shot only"))
    }
}

/// The layer-1 confined-process executor.
///
/// On macOS this renders a Seatbelt profile from the policy and spawns
/// commands under it; on Linux it renders the policy into a bubblewrap command
/// or, when bubblewrap is unavailable, a Landlock ruleset. On other platforms
/// construction fails closed — the sandbox never spawns a command without OS
/// confinement.
#[cfg(target_os = "macos")]
pub use crate::seatbelt::ConfinedProcessExecutor;

#[cfg(target_os = "linux")]
pub use crate::linux::ConfinedProcessExecutor;

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
#[derive(Debug, Clone, Copy)]
pub struct ConfinedProcessExecutor;

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
impl ConfinedProcessExecutor {
    /// Always fails: there is no OS confinement layer on this platform yet.
    ///
    /// # Errors
    ///
    /// Returns [`SandboxError::UnsupportedPlatform`] unconditionally.
    pub fn new(_policy: &crate::policy::Policy) -> Result<Self, crate::error::SandboxError> {
        Err(crate::error::SandboxError::UnsupportedPlatform(
            "the confined-process executor is implemented for macOS and Linux only so far",
        ))
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
#[async_trait]
impl Executor for ConfinedProcessExecutor {
    async fn exec(
        &self,
        _command: &str,
    ) -> ExecResult {
        ExecResult {
            stdout: String::new(),
            stderr: String::from("[agentd-sandbox] no confinement layer on this platform"),
            exit_code: 126,
            denied: false,
        }
    }
}
