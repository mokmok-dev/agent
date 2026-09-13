//! The executor abstraction: the seam between the sandbox and whatever runs
//! the commands.

use std::fmt::{self, Display, Formatter};

use async_trait::async_trait;

/// Why the policy refused a command before execution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DenialReason {
    /// No allowlist prefix matched the command, or the command was malformed.
    CommandNotAllowed,
    /// The sandbox reached its `max_command_count` guard.
    CommandCountExceeded,
}

impl DenialReason {
    /// The stable string used in event data.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::CommandNotAllowed => "command_not_allowed",
            Self::CommandCountExceeded => "command_count_exceeded",
        }
    }
}

impl Display for DenialReason {
    fn fmt(
        &self,
        formatter: &mut Formatter<'_>,
    ) -> fmt::Result {
        let reason = match self {
            Self::CommandNotAllowed => "command not allowed by the shell policy",
            Self::CommandCountExceeded => "command count limit reached",
        };
        formatter.write_str(reason)
    }
}

/// The outcome of running a command under the policy.
///
/// A policy denial is a normal outcome, not an error: `denied_by` is set and
/// the model receives a structured refusal it can react to.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecResult {
    /// Captured standard output.
    pub stdout: String,
    /// Captured standard error.
    pub stderr: String,
    /// The process exit code. Denied commands report `126` without spawning
    /// anything.
    pub exit_code: i32,
    /// Set when the policy refused the command.
    pub denied_by: Option<DenialReason>,
}

impl ExecResult {
    /// A denial result: nothing ran.
    #[must_use]
    pub const fn denied(reason: DenialReason) -> Self {
        Self {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 126,
            denied_by: Some(reason),
        }
    }

    /// Whether the command was refused by the policy.
    #[must_use]
    pub const fn is_denied(&self) -> bool {
        self.denied_by.is_some()
    }
}

/// Runs a command string under the policy bound at construction.
///
/// The policy is bound when the executor is built, not per call, so a running
/// executor cannot widen its own permissions.
#[async_trait]
pub trait Executor: Send + Sync {
    /// Runs `command` and captures its output.
    async fn exec(
        &self,
        command: &str,
    ) -> ExecResult;
}
