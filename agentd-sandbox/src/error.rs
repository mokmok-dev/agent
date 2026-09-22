//! Errors surfaced by the sandbox.

use agentd_events::LogError;
use thiserror::Error as ThisError;

use crate::policy::PolicyError;

/// Errors from constructing a sandbox and its confinement layers.
///
/// Construction fails closed: any invalid policy input aborts before
/// anything can run under it.
#[derive(Debug, ThisError)]
#[non_exhaustive]
pub enum SandboxError {
    /// The policy is malformed: a path entry the confinement backend cannot
    /// express (an unresolvable write root, a deny that covers a needed path,
    /// an unusable workdir).
    #[error("invalid sandbox policy: {0}")]
    InvalidPolicy(String),
    /// The policy failed its own structural validation, so it is malformed
    /// independent of the platform.
    #[error(transparent)]
    Policy(#[from] PolicyError),
    /// The confinement executor does not exist for the current platform; the
    /// sandbox refuses to run commands without OS confinement.
    #[error("sandbox commands are not supported on this platform: {0}")]
    UnsupportedPlatform(&'static str),
    /// An event could not be durably appended to the log, so the audit trail
    /// for the command would have a hole.
    #[error(transparent)]
    Publish(#[from] LogError),
    /// The command was not allowed to run: a policy rule refused it, an approver
    /// denied it, or nobody decided before the deadline. The log distinguishes
    /// the three (`sandbox.permission.denied` and `.cancelled`), so a caller
    /// that must tell them apart reads the decision rather than this error.
    #[error("the command was denied: the policy or an approver refused it, or no decision arrived")]
    Denied,
    /// A long-lived process could not be spawned.
    #[error(transparent)]
    Spawn(#[from] crate::executor::SpawnError),
    /// An underlying I/O error.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
