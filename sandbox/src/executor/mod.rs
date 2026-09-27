//! Running a policy-confined command and collecting its terminal state.
//!
//! The executor is the only part of the crate that spawns a process. It renders
//! the [`Policy`](crate::policy::Policy) through a [`Backend`], runs the result,
//! and reports the exit code, duration, and output. A one-shot command is [`run`];
//! a long-lived process is [`Process`], which holds a handle the daemon can
//! observe and stop.
//!
//! Two properties are enforced here, not left to the policy:
//!
//! - **The boundary is the kernel's.** The command runs inside the backend's
//!   namespace, so it cannot leave the policy even by ignoring it.
//! - **A stuck command is killed with its descendants.** The backend runs the
//!   command as its PID namespace's init, so a single `SIGKILL` to the backend
//!   brings down everything the command started. Output is drained on separate
//!   threads while the command runs, so a command that writes more than a pipe
//!   buffer cannot deadlock against the reader.

use std::ffi::OsString;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use crate::filesystem::{Backend, RenderError};
use crate::policy::Policy;

mod process;

pub use process::{Process, ProcessOutcome};

/// A command to run under a policy.
#[derive(Debug, Clone)]
pub struct ExecRequest {
    /// The program to run, as resolved by the backend inside the sandbox.
    pub program: OsString,
    /// The program's arguments.
    pub args: Vec<OsString>,
    /// The confinement policy.
    pub policy: Policy,
    /// The egress launch, when the policy grants egress over a Unix socket. The
    /// daemon resolves the binaries and supplies the per-proxy token; the
    /// executor then runs the supervisor as the sandbox's init and injects
    /// `HTTP_PROXY`.
    pub egress: Option<EgressLaunch>,
}

/// The daemon-resolved binary paths and token for an egress grant.
///
/// The daemon resolves these on the **trusted** side, applying the same rule as
/// the backend: a binary inside a policy write root is rejected. The executor does
/// not search. See `docs/sandbox/supervisor.md`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EgressLaunch {
    /// The `sandbox-supervisor` binary that runs as the sandbox's init.
    pub supervisor: PathBuf,
    /// The `egress-forward` binary the supervisor starts.
    pub forwarder: PathBuf,
    /// The per-proxy token, for the `HTTP_PROXY` URL.
    pub token: String,
}

/// The terminal state of a confined command.
///
/// Output is truncated to the policy's `max_output_bytes`: bytes past the cap
/// are read and discarded, so a command that writes a lot still finishes and the
/// daemon's memory stays bounded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecOutcome {
    /// The process exit code, or `None` when a signal ended it.
    pub code: Option<i32>,
    /// How long the command ran.
    pub duration: Duration,
    /// The captured standard output.
    pub stdout: Vec<u8>,
    /// The captured standard error.
    pub stderr: Vec<u8>,
    /// Whether the wall-clock timeout killed the command.
    pub timed_out: bool,
}

/// Why a command could not be run.
#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    /// The policy could not be rendered onto this host.
    #[error(transparent)]
    Render(#[from] RenderError),
    /// No backend can confine a command here, so none was run.
    ///
    /// This is the fail-closed path: a host without a usable mechanism runs the
    /// command nowhere rather than running it unconfined.
    #[error("no confinement backend is available on this host")]
    NoBackend,
    /// A scratch directory could not be created.
    #[error("scratch directory error: {0}")]
    Scratch(#[source] io::Error),
    /// The backend process could not be started.
    #[error("could not start the confined command: {0}")]
    Spawn(#[source] io::Error),
    /// The backend process could not be waited on.
    #[error("could not wait for the confined command: {0}")]
    Wait(#[source] io::Error),
}

/// A scratch directory used as the command's `TMPDIR` and removed on drop.
///
/// It is an empty host directory only so the backend has a mountpoint to turn
/// into a tmpfs; the command's writes there never reach the host.
#[derive(Debug)]
pub struct Scratch(PathBuf);

impl Scratch {
    /// Create a unique, empty scratch directory under `parent`.
    ///
    /// # Errors
    ///
    /// Returns the underlying [`io::Error`] if the directory cannot be created.
    pub fn new(parent: &Path) -> Result<Self, io::Error> {
        let dir = parent.join(format!("agent-scratch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;
        Ok(Self(dir))
    }

    /// The scratch directory's path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Run `request` under `backend`, killing it at the policy's timeout.
///
/// # Errors
///
/// Returns [`ExecError::NoBackend`] when `backend` cannot confine a command,
/// [`ExecError::Render`] when the policy cannot be rendered, or a spawn/wait
/// error from the underlying process.
pub fn run(
    backend: &Backend,
    request: &ExecRequest,
    scratch: &Scratch,
) -> Result<ExecOutcome, ExecError> {
    let started = Instant::now();
    // The proxy variables are injected into a clone of the policy, so the caller's
    // request is untouched and the environment the command receives names the
    // forwarder on loopback.
    let request = with_proxy_env(request);
    let mut child = start_confined(backend, &request, scratch.path(), Output::Piped)?;

    // Drain both pipes on their own threads, so a command that writes more than
    // the pipe buffer holds cannot block the process that is waiting for it. The
    // cap is passed in so a runaway command cannot grow the buffer without bound.
    let cap = request.policy.limits.max_output_bytes;
    let out_reader = child.stdout.take().map(|pipe| spawn_reader(pipe, cap));
    let err_reader = child.stderr.take().map(|pipe| spawn_reader(pipe, cap));

    let (status, timed_out) = wait_with_timeout(&mut child, request.policy.limits.timeout())?;

    let stdout = out_reader.map(join_reader).unwrap_or_default();
    let stderr = err_reader.map(join_reader).unwrap_or_default();

    Ok(ExecOutcome {
        code: status.code(),
        duration: started.elapsed(),
        stdout,
        stderr,
        timed_out,
    })
}

/// The confined command's standard streams.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Output {
    /// Capture stdout and stderr on pipes, for a one-shot command whose output is
    /// returned.
    Piped,
    /// Inherit the daemon's streams, for a long-lived process whose output is not
    /// captured.
    Inherited,
}

/// Build and start the confined process under `backend`.
///
/// When `request.egress` is set, the backend runs the egress **supervisor** as
/// the sandbox's init instead of the command, and the proxy variables are injected
/// into the policy's environment first. The supervisor then starts the forwarder
/// and the command in the same namespace. Otherwise the backend runs the command
/// directly.
pub(super) fn start_confined(
    backend: &Backend,
    request: &ExecRequest,
    scratch: &Path,
    output: Output,
) -> Result<Child, ExecError> {
    let (program, args) = match (backend, backend.render(&request.policy, scratch)?) {
        (Backend::Bubblewrap { program }, Some(args)) => (program.clone(), args),
        // An unsupported backend renders nothing; running is refused.
        _ => return Err(ExecError::NoBackend),
    };

    let mut builder = Command::new(&program);
    let init = init_args(request);
    builder.args(&args).arg("--");
    builder.args(&init);

    // The backend sets the confined environment from the policy; its own
    // environment is cleared too, so no host variable reaches the command.
    builder
        .env_clear()
        .stdin(Stdio::null())
        .stdout(match output {
            Output::Piped => Stdio::piped(),
            Output::Inherited => Stdio::inherit(),
        })
        .stderr(match output {
            Output::Piped => Stdio::piped(),
            Output::Inherited => Stdio::inherit(),
        })
        .spawn()
        .map_err(ExecError::Spawn)
}

/// The program `bwrap` runs as the sandbox's init and its arguments.
///
/// Without egress this is the command itself. With egress it is the **supervisor**,
/// which starts the forwarder and the command in the same namespace:
/// `<supervisor> --forward <f> --socket <s> --port <n> -- <cmd> <args>`. The argv
/// is built here, not by the renderer, because it is an executor concern, not a
/// filesystem one.
fn init_args(request: &ExecRequest) -> Vec<OsString> {
    request.egress.as_ref().map_or_else(
        || {
            std::iter::once(request.program.clone())
                .chain(request.args.iter().cloned())
                .collect()
        },
        |egress| {
            let grant = request.policy.network.proxy.as_ref();
            let socket = grant
                .and_then(|grant| grant.socket.as_deref())
                .map_or_else(OsString::new, |path| path.as_os_str().to_os_string());
            let port = grant.map_or(0, |grant| grant.port);
            let mut args = vec![
                egress.supervisor.clone().into_os_string(),
                OsString::from("--forward"),
                egress.forwarder.clone().into_os_string(),
                OsString::from("--socket"),
                socket,
                OsString::from("--port"),
                OsString::from(port.to_string()),
                OsString::from("--"),
                request.program.clone(),
            ];
            args.extend(request.args.iter().cloned());
            args
        },
    )
}

/// Clone `request`, injecting the proxy environment when egress is granted.
///
/// The variables name the forwarder's loopback port, which the policy's
/// `ProxyGrant` already carries. Applying them here, rather than having the
/// renderer emit `--setenv` for them, keeps the environment a policy concern and
/// the launch an executor concern.
pub(super) fn with_proxy_env(request: &ExecRequest) -> ExecRequest {
    // The common case has no egress, so return a clone without touching the
    // environment rather than cloning-then-checking.
    let Some(egress) = &request.egress else {
        return request.clone();
    };
    let port = request
        .policy
        .network
        .proxy
        .as_ref()
        .map_or(0, |grant| grant.port);
    let mut request = request.clone();
    crate::egress::inject_proxy_env(port, &egress.token, &mut request.policy.shell.env);
    request
}

/// Wait for `child`, killing it if it exceeds `timeout`.
///
/// Returns the status and whether the timeout fired. The kill is a `SIGKILL` to
/// the backend, which runs the command as its PID namespace's init; the kernel
/// then kills every process in that namespace, so descendants cannot survive.
fn wait_with_timeout(
    child: &mut Child,
    timeout: Duration,
) -> Result<(ExitStatus, bool), ExecError> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().map_err(ExecError::Wait)? {
            return Ok((status, false));
        }
        if Instant::now() >= deadline {
            child.kill().map_err(ExecError::Wait)?;
            let status = child.wait().map_err(ExecError::Wait)?;
            return Ok((status, true));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Start a thread that reads a pipe, keeping at most `cap` bytes.
///
/// Bytes past the cap are still read and discarded, so the command never blocks
/// on a full pipe and always finishes; only the retained buffer is bounded, so a
/// runaway command cannot grow the daemon's memory without bound.
fn spawn_reader(
    mut pipe: impl Read + Send + 'static,
    cap: u64,
) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let cap = usize::try_from(cap).unwrap_or(usize::MAX);
        let mut retained = Vec::new();
        let mut buffer = [0_u8; 8192];
        loop {
            match pipe.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                // Append whatever arrived, then drop everything past the cap.
                // Truncating after the extend keeps the retained buffer bounded
                // without a guard that an equivalent mutation could survive.
                Ok(read) => {
                    retained.extend_from_slice(&buffer[..read]);
                    retained.truncate(cap);
                },
            }
        }
        retained
    })
}

/// Join a reader thread, yielding what it read, or nothing if it panicked.
fn join_reader(handle: std::thread::JoinHandle<Vec<u8>>) -> Vec<u8> {
    handle.join().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    // Tests for the executor's non-spawning parts: the scratch directory's
    // lifecycle and the timeout loop. The loop is tested with a plain child, not
    // a confined one, so it is covered even on a host that cannot build a
    // namespace (a CI runner, for instance); the bubblewrap spawn tests are
    // integration tests.

    use super::*;

    /// A minimal request running `/bin/echo hi`.
    fn request(
        egress: Option<EgressLaunch>,
        proxy: Option<crate::policy::ProxyGrant>,
    ) -> ExecRequest {
        use crate::policy::{FsEntry, FsPolicy, ShellPolicy};
        ExecRequest {
            program: OsString::from("/bin/echo"),
            args: vec![OsString::from("hi")],
            policy: Policy {
                fs: FsPolicy {
                    entries: vec![FsEntry::write("/work")],
                    protected: Vec::new(),
                },
                shell: ShellPolicy {
                    env: Vec::new(),
                    workdir: PathBuf::from("/work"),
                },
                network: crate::policy::NetworkPolicy {
                    proxy,
                    ..crate::policy::NetworkPolicy::default()
                },
                ..Policy::default()
            },
            egress,
        }
    }

    #[test]
    fn without_egress_the_init_is_the_command() {
        let args = init_args(&request(None, None));
        assert_eq!(
            args,
            vec![OsString::from("/bin/echo"), OsString::from("hi")]
        );
    }

    #[test]
    fn with_egress_the_init_is_the_supervisor() {
        let egress = EgressLaunch {
            supervisor: PathBuf::from("/sb/bin/sandbox-supervisor"),
            forwarder: PathBuf::from("/sb/bin/egress-forward"),
            token: "tok".to_owned(),
        };
        let grant = crate::policy::ProxyGrant {
            port: 8080,
            socket: Some(PathBuf::from("/run/egress.sock")),
            egress: Vec::new(),
        };
        let args = init_args(&request(Some(egress), Some(grant)));
        assert_eq!(
            args,
            vec![
                OsString::from("/sb/bin/sandbox-supervisor"),
                OsString::from("--forward"),
                OsString::from("/sb/bin/egress-forward"),
                OsString::from("--socket"),
                OsString::from("/run/egress.sock"),
                OsString::from("--port"),
                OsString::from("8080"),
                OsString::from("--"),
                OsString::from("/bin/echo"),
                OsString::from("hi"),
            ]
        );
    }

    #[test]
    fn with_egress_the_proxy_environment_is_injected() {
        let egress = EgressLaunch {
            supervisor: PathBuf::from("/s"),
            forwarder: PathBuf::from("/f"),
            token: "tok".to_owned(),
        };
        let grant = crate::policy::ProxyGrant {
            port: 9000,
            socket: Some(PathBuf::from("/run/e.sock")),
            egress: Vec::new(),
        };
        let request = with_proxy_env(&request(Some(egress), Some(grant)));
        let http = request
            .policy
            .shell
            .env
            .iter()
            .find(|var| var.name == "HTTP_PROXY")
            .expect("HTTP_PROXY is set");
        assert_eq!(http.value, "http://agent:tok@127.0.0.1:9000");
    }
    #[test]
    fn without_egress_no_proxy_environment_is_added() {
        let request = with_proxy_env(&request(None, None));
        assert!(request.policy.shell.env.is_empty());
    }

    #[test]
    fn a_scratch_directory_is_created_and_removed_on_drop() {
        let parent = std::env::temp_dir().join(format!("sandbox-scratch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&parent);
        std::fs::create_dir_all(&parent).expect("creates the parent");

        let path;
        {
            let scratch = Scratch::new(&parent).expect("creates the scratch");
            path = scratch.path().to_path_buf();
            assert!(path.is_dir(), "the scratch directory exists while alive");
        }
        assert!(
            !path.exists(),
            "the scratch directory must be removed when the guard drops"
        );

        let _ = std::fs::remove_dir_all(&parent);
    }

    /// Spawn `/bin/sh -c <script>`, or `None` when there is no `/bin/sh`.
    fn shell(script: &str) -> Option<Child> {
        if !Path::new("/bin/sh").exists() {
            return None;
        }
        Command::new("/bin/sh")
            .args(["-c", script])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()
    }

    #[test]
    fn a_command_that_finishes_before_the_timeout_is_not_killed() {
        // The child is still running at the first `try_wait`, so a deadline
        // computed in the past, or an inverted comparison, would kill it early
        // and report `timed_out`.
        let Some(mut child) = shell("sleep 0.3") else {
            return;
        };
        let (status, timed_out) =
            wait_with_timeout(&mut child, Duration::from_secs(30)).expect("waits");
        assert!(status.success(), "the command should exit cleanly");
        assert!(
            !timed_out,
            "a command within the timeout must not be killed"
        );
    }

    #[test]
    fn a_command_that_exceeds_the_timeout_is_killed() {
        let Some(mut child) = shell("sleep 30") else {
            return;
        };
        let (_, timed_out) =
            wait_with_timeout(&mut child, Duration::from_millis(100)).expect("waits");
        assert!(timed_out, "a command past the timeout must be killed");
    }
}
