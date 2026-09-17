//! The shared confined-process runtime: the parts of running a command that do
//! not depend on how the policy is rendered into an OS mechanism.
//!
//! A platform executor renders the policy into a `std::process::Command` and
//! hands it to [`run`] or [`spawn`]. Both capture stdout/stderr under one shared
//! byte budget, enforce the wall-clock timeout, and kill the process group so a
//! runaway tree cannot outlive the command it came from.

use std::fs;
use std::io;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time::timeout;

use crate::error::SandboxError;
use crate::executor::{ExecResult, SpawnError};
use crate::policy::{Access, FsPolicy};

/// A command the executor could not start, or that failed before a real exit.
pub const EXIT_CANNOT_EXECUTE: i32 = 126;
/// A command that outlived the wall-clock timeout.
pub const EXIT_TIMED_OUT: i32 = 124;
/// A command killed by `SIGKILL` (a timeout or output-cap kill).
pub const SIGKILL_EXIT: i32 = 128 + 9;

/// The system directories the confined `PATH` is built from.
pub const SYSTEM_BIN_DIRS: &[&str] = &[
    "/bin",
    "/sbin",
    "/usr/bin",
    "/usr/sbin",
    "/usr/local/bin",
    "/opt/homebrew/bin",
];

/// The shell a confined command runs under, like every backend's `-c` wrapper.
pub const BASH: &str = "/bin/bash";

/// A refusal result for a command that never ran.
#[must_use]
pub fn cannot_execute(message: &str) -> ExecResult {
    ExecResult {
        stdout: String::new(),
        stderr: format!("[agentd-sandbox] {message}"),
        exit_code: EXIT_CANNOT_EXECUTE,
        denied: false,
    }
}

/// Runs `std_command`, capturing output under the byte cap and the timeout.
///
/// `std_command` must already carry the confinement and the environment; this
/// only wires stdio, runs, and maps the outcome to an [`ExecResult`] with the
/// documented exit conventions.
pub async fn run(
    mut std_command: std::process::Command,
    wall_clock: Duration,
    max_output_bytes: u64,
) -> ExecResult {
    std_command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = match Command::from(std_command).spawn() {
        Ok(child) => child,
        Err(error) => {
            return cannot_execute(&format!("spawning the confined command failed: {error}"));
        },
    };
    let pid = child.id();

    let mut stdout_data = Vec::new();
    let mut stderr_data = Vec::new();
    let remaining = AtomicU64::new(max_output_bytes);
    let truncated = AtomicBool::new(false);
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();

    let wait = async {
        let stdout_state = Capture {
            data: &mut stdout_data,
            remaining: &remaining,
            truncated: &truncated,
            pid,
        };
        let stderr_state = Capture {
            data: &mut stderr_data,
            remaining: &remaining,
            truncated: &truncated,
            pid,
        };
        let (stdout_read, stderr_read) = tokio::join!(
            capture(stdout_pipe, stdout_state),
            capture(stderr_pipe, stderr_state),
        );
        match (stdout_read, stderr_read) {
            (Ok(()), Ok(())) => child.wait().await,
            (Err(error), _) | (_, Err(error)) => Err(error),
        }
    };

    let (mut exit_code, timed_out) = match timeout(wall_clock, Box::pin(wait)).await {
        Ok(Ok(status)) => (
            status.code().unwrap_or_else(|| {
                status
                    .signal()
                    .map_or(EXIT_CANNOT_EXECUTE, |signal| 128 + signal)
            }),
            false,
        ),
        Ok(Err(error)) => {
            kill_group(pid);
            let _ = child.wait().await;
            return cannot_execute(&format!("waiting for the command failed: {error}"));
        },
        Err(_) => {
            kill_group(pid);
            let _ = timeout(Duration::from_secs(5), child.wait()).await;
            (EXIT_TIMED_OUT, true)
        },
    };
    // The capture task already killed the process group when the cap was hit;
    // the child is reaped by now, so the pid must not be signalled again (it
    // may belong to someone else).
    if truncated.load(Ordering::Relaxed) {
        exit_code = exit_code.max(SIGKILL_EXIT);
    }

    let stdout = String::from_utf8_lossy(&stdout_data).into_owned();
    let mut stderr = String::from_utf8_lossy(&stderr_data).into_owned();
    if timed_out {
        stderr.push_str("\n[agentd-sandbox] command exceeded the wall-clock timeout");
    }
    if truncated.load(Ordering::Relaxed) {
        stderr.push_str("\n[agentd-sandbox] output truncated at max_output_bytes");
    }
    ExecResult {
        stdout,
        stderr,
        exit_code,
        denied: false,
    }
}

/// Spawns `std_command` long-lived with piped stdio, without waiting.
///
/// `std_command` must already carry the confinement and the environment.
///
/// # Errors
///
/// Returns [`SpawnError::Io`] when the process cannot be started.
pub fn spawn(mut std_command: std::process::Command) -> Result<tokio::process::Child, SpawnError> {
    std_command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = Command::from(std_command).spawn()?;
    Ok(child)
}

/// Kills the process group led by `pid`, if any.
pub fn kill_group(pid: Option<u32>) {
    let Some(pid) = pid else {
        return;
    };
    let Ok(raw) = i32::try_from(pid) else {
        return;
    };
    let _ = killpg(Pid::from_raw(raw), Signal::SIGKILL);
}

/// One output-capture pipe's read loop: appends up to the shared remaining
/// byte budget, then kills the process group so the producer cannot fill the
/// pipe forever.
struct Capture<'a> {
    data: &'a mut Vec<u8>,
    remaining: &'a AtomicU64,
    truncated: &'a AtomicBool,
    pid: Option<u32>,
}

async fn capture<R: tokio::io::AsyncRead + Unpin>(
    pipe: Option<R>,
    state: Capture<'_>,
) -> io::Result<()> {
    let Some(mut pipe) = pipe else {
        return Ok(());
    };
    let mut buffer = [0_u8; 8192];
    loop {
        let read = pipe.read(&mut buffer).await?;
        if read == 0 {
            return Ok(());
        }
        let allowed = usize::try_from(state.remaining.load(Ordering::Relaxed))
            .unwrap_or(usize::MAX)
            .min(read);
        state.data.extend_from_slice(&buffer[..allowed]);
        state.remaining.fetch_sub(allowed as u64, Ordering::Relaxed);
        if allowed < read {
            state.truncated.store(true, Ordering::Relaxed);
            kill_group(state.pid);
            return Ok(());
        }
    }
}

/// Validates the workdir and returns its canonical form.
///
/// The workdir must be absolute, exist as a directory, and be covered by a
/// `write` entry: a workdir the process cannot write is not a workspace. A
/// workdir outside every write root is rejected rather than silently running
/// read-only.
pub fn validate_workdir(
    workdir: &Path,
    fs_policy: &FsPolicy,
) -> Result<PathBuf, SandboxError> {
    if !workdir.is_absolute() {
        return Err(SandboxError::InvalidPolicy(format!(
            "workdir {:?} must be absolute",
            workdir.display().to_string()
        )));
    }
    let metadata = fs::metadata(workdir).map_err(|error| {
        SandboxError::InvalidPolicy(format!(
            "workdir {} is not accessible: {error}",
            workdir.display()
        ))
    })?;
    if !metadata.is_dir() {
        return Err(SandboxError::InvalidPolicy(format!(
            "workdir {} is not a directory",
            workdir.display()
        )));
    }
    let canonical = workdir.canonicalize()?;
    let covered = fs_policy.entries.iter().any(|entry| {
        entry.access == Access::Write
            && entry
                .path
                .canonicalize()
                .is_ok_and(|root| canonical.starts_with(&root))
    });
    if !covered {
        return Err(SandboxError::InvalidPolicy(format!(
            "workdir {} is not inside a write entry",
            workdir.display()
        )));
    }
    Ok(canonical)
}

/// Creates the per-executor scratch directory under the system temp dir.
///
/// # Errors
///
/// Returns [`SandboxError::Io`] when it cannot be created.
pub fn create_scratch() -> Result<PathBuf, SandboxError> {
    let scratch = std::env::temp_dir().join(format!("agentd-sandbox-{}", uuid::Uuid::now_v7()));
    fs::create_dir_all(&scratch)?;
    Ok(scratch)
}

/// The system directories that exist, as the confined `PATH`.
pub fn system_path_dirs() -> Vec<PathBuf> {
    SYSTEM_BIN_DIRS
        .iter()
        .filter(|dir| Path::new(dir).is_dir())
        .map(PathBuf::from)
        .collect()
}

/// Joins `path_dirs` into one `PATH` value.
///
/// # Errors
///
/// Returns [`SandboxError::InvalidPolicy`] when a directory contains a
/// separator-invalid character.
pub fn join_path(path_dirs: &[PathBuf]) -> Result<String, SandboxError> {
    std::env::join_paths(path_dirs.iter().map(Path::new))
        .map(|paths| paths.to_string_lossy().into_owned())
        .map_err(|error| {
            SandboxError::InvalidPolicy(format!("confined PATH is malformed: {error}"))
        })
}
