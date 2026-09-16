//! The executor abstraction: the seam between the sandbox and whatever runs
//! the commands.
//!
//! There is exactly one production implementation (the layer-1 confined-process
//! executor); the trait exists so tests can inject a fake without spawning a
//! process. The policy is bound at construction, not per call, so a running
//! executor cannot widen its own permissions.

use async_trait::async_trait;

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
}

/// The layer-1 confined-process executor.
///
/// On macOS this renders a Seatbelt profile from the policy and spawns
/// commands under it. On other platforms construction fails closed — the
/// sandbox never spawns a command without OS confinement — and the Linux
/// (bubblewrap/Landlock/seccomp) backend is a planned follow-up.
#[cfg(target_os = "macos")]
pub use crate::seatbelt::ConfinedProcessExecutor;

#[cfg(not(target_os = "macos"))]
#[derive(Debug, Clone, Copy)]
pub struct ConfinedProcessExecutor;

#[cfg(not(target_os = "macos"))]
impl ConfinedProcessExecutor {
    /// Always fails: there is no OS confinement layer on this platform yet.
    ///
    /// # Errors
    ///
    /// Returns [`SandboxError::UnsupportedPlatform`] unconditionally.
    pub fn new(_policy: &crate::policy::Policy) -> Result<Self, crate::error::SandboxError> {
        Err(crate::error::SandboxError::UnsupportedPlatform(
            "the confined-process executor is implemented for macOS only so far",
        ))
    }
}

#[cfg(not(target_os = "macos"))]
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
