//! Spawning the confined process a session owns.
//!
//! The manager starts a session's process through a [`Launcher`], which is a
//! seam rather than a direct call to the sandbox. The real launcher renders the
//! policy through the sandbox's executor; a test supplies a fake one, so the
//! start sequence, the lifecycle, and the teardown are all exercisable on a host
//! that cannot build a namespace (such as a CI runner, which forbids
//! unprivileged user namespaces).
//!
//! This module defines the seam. The real [`Launcher`] over the sandbox's
//! executor, and the egress helper resolution it needs, arrive with the milestone
//! that spawns a confined agent end to end, because both need a host that can
//! build a namespace.

use std::path::PathBuf;

use sandbox::executor::EgressLaunch;
use sandbox::policy::Policy;

use crate::Error;

/// A running confined process, as the manager observes it.
///
/// The manager needs only to ask whether it is alive and to stop it; the
/// sandbox's `Process` implements this, and a test supplies a stub.
pub trait LaunchedProcess: Send + std::fmt::Debug {
    /// Whether the process is still running.
    ///
    /// # Errors
    ///
    /// Returns the underlying I/O error if the check fails.
    fn is_running(&mut self) -> std::io::Result<bool>;

    /// Stop the process and its descendants.
    ///
    /// # Errors
    ///
    /// Returns the underlying I/O error if the kill fails.
    fn kill(&mut self) -> std::io::Result<()>;
}

/// What a launcher is given to start one session's process.
#[derive(Debug)]
pub struct LaunchRequest {
    /// The confinement policy, already validated and complete (the egress socket
    /// and the bus socket are in it).
    pub policy: Policy,
    /// The program to run.
    pub program: PathBuf,
    /// The program's arguments.
    pub args: Vec<String>,
    /// The scratch directory the executor mounts as the process's `TMPDIR`.
    pub scratch: PathBuf,
    /// The egress launch, when the policy grants egress.
    pub egress: Option<EgressLaunch>,
}

/// The seam the manager starts a session's confined process through.
pub trait Launcher: Send + Sync + std::fmt::Debug {
    /// Whether this host can confine a command. A manager refuses to open a
    /// session when it cannot, so the failure is fail-closed rather than
    /// unconfined.
    fn is_supported(&self) -> bool;

    /// Start the confined process for `request`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Launcher`] when no backend can confine the command or the
    /// spawn fails.
    fn launch(
        &self,
        request: &LaunchRequest,
    ) -> Result<Box<dyn LaunchedProcess>, Error>;
}
