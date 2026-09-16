//! Errors surfaced by the sandbox.

use agentd_events::LogError;
use thiserror::Error as ThisError;

/// Errors from constructing a sandbox and its confinement layers.
///
/// Construction fails closed: any invalid policy input aborts before
/// anything can run under it.
#[derive(Debug, ThisError)]
pub enum SandboxError {
    /// The policy is malformed: invalid glob patterns, mount points, or host
    /// directories.
    #[error("invalid sandbox policy: {0}")]
    InvalidPolicy(String),
    /// The confinement executor does not exist for the current platform; the
    /// sandbox refuses to run commands without OS confinement.
    #[error("sandbox commands are not supported on this platform: {0}")]
    UnsupportedPlatform(&'static str),
    /// An event could not be durably appended to the log, so the audit trail
    /// for the command would have a hole.
    #[error(transparent)]
    Publish(#[from] LogError),
    /// An approver denied the request, so no process was started.
    #[error("the command was denied by the approver")]
    Denied,
    /// A long-lived session could not be spawned.
    #[error(transparent)]
    Spawn(#[from] crate::executor::SpawnError),
    /// An underlying I/O error.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
