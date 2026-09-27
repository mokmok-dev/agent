//! Running a policy-confined command and collecting its terminal state.
//!
//! The executor is the only part of the crate that spawns a process. It renders
//! the [`Policy`](crate::policy::Policy) through a [`Backend`], runs the result,
//! and reports the exit code, duration, and output.
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

/// A command to run under a policy.
#[derive(Debug, Clone)]
pub struct ExecRequest {
    /// The program to run, as resolved by the backend inside the sandbox.
    pub program: OsString,
    /// The program's arguments.
    pub args: Vec<OsString>,
    /// The confinement policy.
    pub policy: Policy,
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
    let (program, args) = match (backend, backend.render(&request.policy, scratch.path())?) {
        (Backend::Bubblewrap { program }, Some(args)) => (program.clone(), args),
        // An unsupported backend renders nothing; running is refused.
        _ => return Err(ExecError::NoBackend),
    };

    let started = Instant::now();
    let mut child = spawn(&program, &args, request)?;

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

/// Build and start the confined process, with piped output.
fn spawn(
    program: &Path,
    args: &[OsString],
    request: &ExecRequest,
) -> Result<Child, ExecError> {
    Command::new(program)
        .args(args)
        .arg("--")
        .arg(&request.program)
        .args(&request.args)
        // The backend sets the confined environment from the policy. Its own
        // environment is cleared too, so no host variable can reach the command
        // through the backend.
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(ExecError::Spawn)
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
    // lifecycle. The spawn tests are integration tests, because they need a real
    // bubblewrap.

    use super::*;

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
}
