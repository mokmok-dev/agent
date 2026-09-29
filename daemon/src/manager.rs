//! The session manager: opening and stopping sessions.
//!
//! Implements the start sequence and teardown from `docs/session/daemon.md` and
//! `docs/session/lifecycle.md`. The manager owns the session registry and the
//! launcher; it is the trusted side of every session.
//!
//! The manager starts a session's process through the [`Launcher`] seam, so the
//! start sequence is testable on a host that cannot build a namespace. The egress
//! proxy's serve loop and the real `SandboxLauncher` composition arrive with the
//! milestone that drives a confined agent end to end.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sandbox::egress::HostCapability;
use serde_json::json;

use crate::Error;
use crate::bus::BusClient;
use crate::egress::Egress;
use crate::image::AgentImage;
use crate::launcher::LaunchRequest;
use crate::session::{Session, SessionId, SessionRegistry, State};

/// The event published when a session's agent is running.
pub const SESSION_STARTED: &str = "agent.session.started";
/// The event published when a session was stopped on request.
pub const SESSION_STOPPED: &str = "agent.session.stopped";
/// The event published when a session's agent ended on its own.
pub const SESSION_EXITED: &str = "agent.session.exited";
/// The event published when a session could not start.
pub const SESSION_FAILED: &str = "agent.session.failed";

/// The runtime paths the manager needs.
#[derive(Debug, Clone)]
pub struct ManagerConfig {
    /// The bus's Unix socket, granted to every agent so it can subscribe.
    pub bus_socket: PathBuf,
    /// Where per-session scratch directories are created.
    pub scratch_parent: PathBuf,
    /// Directories masked from every agent, such as the authority socket's.
    pub deny_under: Vec<PathBuf>,
    /// Where per-session egress sockets are created.
    pub run_dir: PathBuf,
    /// How long the egress proxy waits for an approval decision.
    pub approval_deadline: Duration,
    /// The binary resolution inputs, for an image that grants egress: the
    /// daemon's own path, the `PATH`, and the workspace roots to reject.
    pub daemon_exe: PathBuf,
    /// The `PATH` to resolve the egress helpers against.
    pub search_path: String,
}

impl ManagerConfig {
    /// A configuration rooted at `run_dir`, with the bus socket beside it.
    #[must_use]
    pub fn new(
        bus_socket: impl Into<PathBuf>,
        run_dir: impl Into<PathBuf>,
        daemon_exe: impl Into<PathBuf>,
        search_path: impl Into<String>,
    ) -> Self {
        let run_dir = run_dir.into();
        Self {
            bus_socket: bus_socket.into(),
            scratch_parent: run_dir.join("scratch"),
            run_dir,
            deny_under: Vec::new(),
            approval_deadline: Duration::from_secs(60),
            daemon_exe: daemon_exe.into(),
            search_path: search_path.into(),
        }
    }

    /// Mask `path` from every agent, when it exists.
    #[must_use]
    pub fn denying(
        mut self,
        path: impl Into<PathBuf>,
    ) -> Self {
        self.deny_under.push(path.into());
        self
    }

    /// Set how long the egress proxy waits for an approval decision.
    #[must_use]
    pub const fn with_approval_deadline(
        mut self,
        deadline: Duration,
    ) -> Self {
        self.approval_deadline = deadline;
        self
    }
}

/// Opens and stops sessions.
pub struct Manager {
    config: ManagerConfig,
    registry: Mutex<SessionRegistry>,
    launcher: Box<dyn crate::launcher::Launcher>,
    /// The egress proxy's host capability, read from the launcher's backend.
    capability: HostCapability,
    /// The daemon's bus connection, shared with each session's egress proxy.
    bus: Arc<BusClient>,
}

impl std::fmt::Debug for Manager {
    fn fmt(
        &self,
        f: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        f.debug_struct("Manager")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl Manager {
    /// Build a manager over `config`, using `launcher` to start processes.
    ///
    /// The bus is held as an `Arc` because a session's egress proxy publishes its
    /// approval requests through the same connection: there is one bus client for
    /// the daemon, not one per session.
    #[must_use]
    pub fn new(
        config: ManagerConfig,
        launcher: Box<dyn crate::launcher::Launcher>,
        capability: HostCapability,
        bus: Arc<BusClient>,
    ) -> Self {
        Self {
            config,
            registry: Mutex::new(SessionRegistry::new()),
            launcher,
            capability,
            bus,
        }
    }

    /// Open a session from `image` on `workspace`, publishing its lifecycle to
    /// `bus`.
    ///
    /// Runs the start sequence: reserve the id and the workspace, refuse a host
    /// that cannot confine, build and validate the policy, create the scratch,
    /// spawn the confined process, and record the session as running.
    ///
    /// # Errors
    ///
    /// Returns an [`Error`] naming the step that failed. The session is left in
    /// `Failed` with its resources released, and `agent.session.failed` is
    /// published, so a consumer that waits for `started` cannot hang.
    pub fn open_session(
        &self,
        bus: &BusClient,
        id: &SessionId,
        image: &AgentImage,
        workspace: &Path,
    ) -> Result<(), Error> {
        // 1. Reserve the id and the workspace before anything is created.
        {
            let mut registry = self.registry.lock().map_err(|_| Error::Poisoned)?;
            registry.insert(id.clone(), workspace.to_path_buf())?;
        }

        match self.try_open(id, image, workspace) {
            Ok(()) => {
                publish(bus, SESSION_STARTED, id, json!({"workspace": workspace}));
                Ok(())
            },
            Err(error) => {
                if let Ok(mut registry) = self.registry.lock()
                    && let Some(session) = registry.get_mut(id)
                {
                    // `Starting` is the only state a failed open is in, so the
                    // transition cannot fail here.
                    let _ = session.mark_failed();
                }
                publish(
                    bus,
                    SESSION_FAILED,
                    id,
                    json!({"reason": error.to_string()}),
                );
                Err(error)
            },
        }
    }

    /// The start sequence after the reservation.
    fn try_open(
        &self,
        id: &SessionId,
        image: &AgentImage,
        workspace: &Path,
    ) -> Result<(), Error> {
        // 2. A host that cannot confine runs nothing.
        if !self.launcher.is_supported() {
            return Err(Error::NoBackend);
        }

        // 3. A program inside the workspace is refused: a workspace must not
        //    supply the program it confines.
        if image.program.starts_with(workspace) {
            return Err(Error::ProgramInsideWorkspace(image.program.clone()));
        }

        // 5a. The egress proxy, when the image grants egress. It must exist
        //     before the policy renders, because the renderer requires the socket.
        let egress = if image.grants_egress() {
            Some(Egress::start(
                image,
                self.config.run_dir.join(format!("egress-{id}.sock")),
                &self.capability,
                &self.bus,
                id.as_str(),
                self.config.approval_deadline,
            )?)
        } else {
            None
        };

        // 5b. The policy, with the egress grant and the bus socket.
        let mut policy =
            image.policy(workspace, &self.config.bus_socket, &self.config.deny_under)?;
        // The supervisor and forwarder run inside the namespace, so the executor
        // needs their resolved paths. A host without them cannot grant egress, so
        // this fails closed rather than running with no route through the proxy.
        let egress_launch = egress
            .as_ref()
            .map(|active| {
                policy.network.proxy = Some(active.grant(image));
                policy.validate()?;
                let binaries = crate::helper::resolve_egress(
                    &self.config.daemon_exe,
                    &self.config.search_path,
                    &[workspace.to_path_buf()],
                )
                .ok_or(Error::EgressHelpersMissing)?;
                Ok::<_, Error>(sandbox::executor::EgressLaunch {
                    supervisor: binaries.supervisor,
                    forwarder: binaries.forwarder,
                    token: active.token().to_owned(),
                })
            })
            .transpose()?;
        if egress.is_some() {
            policy.validate()?;
        }

        // 5c. The scratch is created before the render, because the renderer
        //     needs it as a mountpoint.
        let scratch = sandbox::executor::Scratch::new(&self.config.scratch_parent)
            .map_err(|error| Error::Scratch(error.to_string()))?;

        let request = LaunchRequest {
            policy,
            program: image.program.clone(),
            args: image.args.clone(),
            scratch: scratch.path().to_path_buf(),
            egress: egress_launch,
        };

        // 6. Spawn the confined process; from here teardown owns it.
        let process = self.launcher.launch(&request)?;

        // 7. Attach the resources and move to running. The egress proxy lives in
        //    the session, so it is released with everything else on teardown.
        {
            let mut registry = self.registry.lock().map_err(|_| Error::Poisoned)?;
            registry.attach(id, scratch, process)?;
            registry.attach_egress(id, egress)?;
            registry.apply(id, Session::mark_running)?;
        }
        Ok(())
    }

    /// Stop a session on request: kill the process, release the scratch, and
    /// record it stopped.
    ///
    /// # Errors
    ///
    /// Returns an [`Error`] if the session is unknown or not running.
    pub fn stop_session(
        &self,
        bus: &BusClient,
        id: &SessionId,
    ) -> Result<(), Error> {
        {
            let mut registry = self.registry.lock().map_err(|_| Error::Poisoned)?;
            registry.apply(id, Session::request_stop)?;
            registry.shutdown(id)?;
            registry.apply(id, Session::mark_stopped)?;
        }
        publish(bus, SESSION_STOPPED, id, json!({}));
        Ok(())
    }

    /// Record that a session's process ended on its own, releasing its resources.
    ///
    /// # Errors
    ///
    /// Returns an [`Error`] if the session is unknown or not running.
    pub fn mark_exited(
        &self,
        bus: &BusClient,
        id: &SessionId,
    ) -> Result<(), Error> {
        {
            let mut registry = self.registry.lock().map_err(|_| Error::Poisoned)?;
            registry.shutdown(id)?;
            registry.apply(id, Session::mark_exited)?;
        }
        publish(bus, SESSION_EXITED, id, json!({}));
        Ok(())
    }

    /// The state of a session.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NoSuchSession`] if unknown.
    pub fn state(
        &self,
        id: &SessionId,
    ) -> Result<State, Error> {
        let registry = self.registry.lock().map_err(|_| Error::Poisoned)?;
        registry
            .get(id)
            .map(Session::state)
            .ok_or_else(|| Error::NoSuchSession(id.clone()))
    }
}

/// Publish one of the manager's own lifecycle events.
///
/// A failure is logged rather than propagated: the session is already in its new
/// state, and an operator must see that the log is missing the event.
fn publish(
    bus: &BusClient,
    ty: &str,
    id: &SessionId,
    data: serde_json::Value,
) {
    if let Err(error) = bus.publish(crate::bus::authored(ty, id.to_string(), data, None)) {
        tracing::error!(%error, %ty, %id, "could not publish a session lifecycle event");
    }
}
