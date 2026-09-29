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

use sandbox::executor::{EgressLaunch, ExecRequest, Process, ProcessStdio};
use sandbox::filesystem::Backend;
use sandbox::policy::Policy;

use crate::Error;

/// A running confined process, as the manager observes it.
///
/// The manager needs only to stop the process; the sandbox's `Process`
/// implements this, and a test supplies a stub.
pub trait LaunchedProcess: Send + std::fmt::Debug {
    /// Stop the process and its descendants.
    ///
    /// # Errors
    ///
    /// Returns the underlying I/O error if the kill fails.
    fn kill(&mut self) -> std::io::Result<()>;
}

impl LaunchedProcess for Process {
    fn kill(&mut self) -> std::io::Result<()> {
        Self::kill(self)
    }
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

/// The real launcher: the sandbox's executor under a detected backend.
#[derive(Debug, Clone)]
pub struct SandboxLauncher {
    backend: Backend,
}

impl SandboxLauncher {
    /// Detect the backend this host can enforce.
    ///
    /// `reject_under` is the policy's writable root, so a `bwrap` inside it is
    /// refused, the same rule the sandbox applies.
    #[must_use]
    pub fn detect(
        search_path: &str,
        reject_under: &[PathBuf],
    ) -> Self {
        Self {
            backend: Backend::detect(search_path, reject_under),
        }
    }

    /// The detected backend, for a caller that needs to read the host's egress
    /// capability before it binds a proxy.
    #[must_use]
    pub const fn backend(&self) -> &Backend {
        &self.backend
    }

    /// A launcher over an explicit backend.
    ///
    /// Lets a caller that already detected a backend reuse it, and lets a test
    /// exercise both the supported and unsupported paths without a host.
    #[must_use]
    pub const fn from_backend(backend: Backend) -> Self {
        Self { backend }
    }
}

impl Launcher for SandboxLauncher {
    fn is_supported(&self) -> bool {
        self.backend.is_supported()
    }

    fn launch(
        &self,
        request: &LaunchRequest,
    ) -> Result<Box<dyn LaunchedProcess>, Error> {
        if !self.backend.is_supported() {
            return Err(Error::NoBackend);
        }
        let exec = ExecRequest {
            program: request.program.clone().into_os_string(),
            args: request.args.iter().map(Into::into).collect(),
            policy: request.policy.clone(),
            egress: request.egress.clone(),
        };
        let process = Process::spawn(
            &self.backend,
            &exec,
            &request.scratch,
            ProcessStdio::Inherited,
        )
        .map_err(|error| Error::Launcher(error.to_string()))?;
        Ok(Box::new(process))
    }
}

#[cfg(test)]
mod tests {
    // Tests that need no confinement: the support flag mirrors the backend, and a
    // launcher with no backend refuses to launch.

    use super::*;

    #[test]
    fn support_mirrors_the_detected_backend() {
        // On this host, whatever the backend, the launcher's support flag must
        // equal it. A constant `true` would claim support on a host without a
        // mechanism; a constant `false` would refuse a host that has one.
        let launcher = SandboxLauncher::detect("", &[]);
        assert_eq!(
            launcher.is_supported(),
            launcher.backend().is_supported(),
            "the launcher's support must mirror the backend"
        );
    }

    #[test]
    fn a_supported_backend_reports_support() {
        // A synthetic supported backend, so the flag is pinned to the backend on
        // a host that has no mechanism at all.
        use sandbox::filesystem::Backend;
        let bubblewrap = Backend::Bubblewrap {
            program: PathBuf::from("/usr/bin/bwrap"),
        };
        assert!(
            SandboxLauncher::from_backend(bubblewrap).is_supported(),
            "a bubblewrap backend is supported"
        );
    }

    #[test]
    fn an_unsupported_backend_reports_no_support() {
        use sandbox::filesystem::{Backend, BackendError};
        let unsupported = Backend::Unsupported {
            reason: BackendError::BubblewrapNotInstalled,
        };
        assert!(
            !SandboxLauncher::from_backend(unsupported).is_supported(),
            "an unsupported backend reports no support"
        );
    }

    #[test]
    fn a_launcher_without_a_backend_refuses_to_launch() {
        // `""` resolves no `bwrap`, so this is the fail-closed path.
        let launcher = SandboxLauncher::detect("", &[]);
        assert!(!launcher.is_supported());
        let request = LaunchRequest {
            policy: sandbox::policy::Policy::default(),
            program: PathBuf::from("/bin/true"),
            args: Vec::new(),
            scratch: PathBuf::from("/tmp"),
            egress: None,
        };
        let error = launcher
            .launch(&request)
            .expect_err("a launcher without a backend must refuse");
        assert!(matches!(error, Error::NoBackend));
    }
}
