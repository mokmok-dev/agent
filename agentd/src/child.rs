//! The bounded child runner shared by every capability that spawns a process.
//!
//! `shell` runs one command; `patch` runs `git apply`. Both need the same
//! invariant the OOM incident established: a child's output is drained on its
//! own threads and **bounded while it is read**, never buffered whole and then
//! measured, and a child that never returns is killed at a deadline with its
//! partial output kept.
//!
//! The runner clears the child's environment and sets only the entries the
//! caller names, because a session's child must not inherit the agent's. That is
//! why a caller names a program by absolute path: nothing resolves from `PATH`.

use std::io::{Read, Write};
use std::path::Path;
use std::process::Command as ChildCommand;
use std::process::{Child, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// What one bounded read retained.
#[derive(Debug, Default)]
pub struct Captured {
    /// The retained head, at most the cap it was read with.
    pub head: Vec<u8>,
    /// How many bytes the child wrote in total, retained or not.
    pub total: u64,
    /// Whether any bytes past the cap were dropped.
    pub truncated: bool,
}

impl Captured {
    /// The retained bytes as lossy UTF-8, for an event's JSON.
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.head).into_owned()
    }
}

/// Read `reader` to its end, retaining at most `cap` bytes.
///
/// Bytes past the cap are still read and discarded, so the child never blocks on
/// a full pipe; only the retained buffer is bounded. A read error ends the read and
/// keeps what arrived, because a broken pipe is not a reason to lose the child's
/// output.
pub fn capture_reader(
    mut reader: impl Read,
    cap: usize,
) -> Captured {
    let mut captured = Captured::default();
    let mut buffer = [0_u8; 8192];
    loop {
        let Ok(read) = reader.read(&mut buffer) else {
            return captured;
        };
        if read == 0 {
            return captured;
        }
        captured.total += read as u64;
        let room = cap.saturating_sub(captured.head.len());
        let keep = room.min(read);
        captured.head.extend_from_slice(&buffer[..keep]);
        if keep < read {
            captured.truncated = true;
        }
    }
}

/// Read `pipe` on its own thread, bounded by `cap`.
pub fn spawn_capture(
    pipe: impl Read + Send + 'static,
    cap: usize,
) -> JoinHandle<Captured> {
    std::thread::spawn(move || capture_reader(pipe, cap))
}

/// Join a reader thread, yielding what it read, or nothing if it panicked.
pub fn join_capture(handle: JoinHandle<Captured>) -> Captured {
    handle.join().unwrap_or_default()
}

/// Wait for `child`, killing it if it exceeds `timeout`.
///
/// Returns the exit code and whether the timeout fired. A killed child reports
/// no code, which the output records alongside `timed_out`.
pub fn wait_with_timeout(
    child: &mut Child,
    timeout: Duration,
) -> (Option<i32>, bool) {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return (status.code(), false),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let status = child.wait().ok();
                    return (status.and_then(|status| status.code()), true);
                }
                std::thread::sleep(Duration::from_millis(5));
            },
            Err(_) => return (None, false),
        }
    }
}

/// `duration` as whole milliseconds, saturating at the `u64` bound.
///
/// A duration that large is unreachable in practice, but saturating keeps the
/// conversion total rather than a silent truncation.
pub fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// Everything one bounded child run produced. `timed_out` keeps the partial
/// output, which `shell`'s cap test asserts.
#[derive(Debug)]
pub struct Outcome {
    /// The exit code, or `None` when the child was killed or the wait failed.
    pub code: Option<i32>,
    /// The bounded standard output.
    pub stdout: Captured,
    /// The bounded standard error.
    pub stderr: Captured,
    /// Whether the deadline fired and the child was killed.
    pub timed_out: bool,
    /// How long the run took, from before the spawn to after the pipes closed.
    pub duration: Duration,
}

/// Spawn `program` (an absolute path) with `args` in `cwd`, clear the
/// environment, set `env` entries, optionally write `stdin` on its own thread,
/// drain both pipes bounded while reading, and kill the child at `timeout`.
///
/// `env` exists because `patch` sets `GIT_CONFIG_NOSYSTEM` and
/// `GIT_CEILING_DIRECTORIES` on an otherwise cleared environment.
///
/// # Errors
///
/// Returns the spawn error when `program` cannot be started; a child that runs
/// and is killed yields `Ok` with `timed_out` set, never an error.
pub fn run_child(
    program: &Path,
    args: &[&str],
    cwd: &Path,
    env: &[(&str, String)],
    stdin: Option<&[u8]>,
    timeout: Duration,
    cap: usize,
) -> std::io::Result<Outcome> {
    let started = Instant::now();
    let mut command = ChildCommand::new(program);
    command
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if stdin.is_some() {
        command.stdin(Stdio::piped());
    } else {
        command.stdin(Stdio::null());
    }
    for (name, value) in env {
        command.env(*name, value);
    }
    let mut child = command.spawn()?;

    std::thread::scope(|scope| {
        let out_reader = child.stdout.take().map(|pipe| spawn_capture(pipe, cap));
        let err_reader = child.stderr.take().map(|pipe| spawn_capture(pipe, cap));

        // Write stdin on its own thread so a child that both reads a large input
        // and writes a large output cannot deadlock. The scope joins it once the
        // child has ended and the pipes have closed.
        if let Some(bytes) = stdin {
            let mut pipe = child.stdin.take();
            scope.spawn(move || {
                if let Some(pipe) = pipe.as_mut() {
                    let _ = pipe.write_all(bytes);
                    let _ = pipe.flush();
                }
            });
        }

        let (code, timed_out) = wait_with_timeout(&mut child, timeout);
        let stdout = out_reader.map(join_capture).unwrap_or_default();
        let stderr = err_reader.map(join_capture).unwrap_or_default();
        Ok(Outcome {
            code,
            stdout,
            stderr,
            timed_out,
            duration: started.elapsed(),
        })
    })
}

#[cfg(test)]
mod tests {
    // Tests for the bounded runner itself. `shell`'s tests cover the extraction's
    // observable behaviour; these cover the two things only this module can prove:
    // that `stdin` reaches the child, and that a killed child still yields its
    // partial capped output rather than an error.

    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use super::*;

    /// Locate a tool by name on the host, the way a shell would. The runner clears
    /// the child's environment, so a test names a tool by absolute path.
    fn helper_path(name: &str) -> Option<PathBuf> {
        std::env::var("PATH")
            .unwrap_or_default()
            .split(':')
            .filter(|dir| !dir.is_empty())
            .map(|dir| Path::new(dir).join(name))
            .find(|candidate| candidate.is_file())
    }

    /// A temp workspace, removed on drop.
    struct Workspace(PathBuf);

    impl Workspace {
        fn new(tag: &str) -> Self {
            static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let unique = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("agent-child-{tag}-{}-{unique}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("creates the workspace");
            Self(path)
        }
    }

    impl Drop for Workspace {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn stdin_written_by_run_child_reaches_the_child() {
        let workspace = Workspace::new("stdin");
        let cat = helper_path("cat").expect("`cat` exists on a supported host");
        let outcome = run_child(
            &cat,
            &[],
            &workspace.0,
            &[],
            Some(b"hello from stdin"),
            Duration::from_secs(30),
            4096,
        )
        .expect("the child starts");

        assert_eq!(outcome.code, Some(0), "`cat` exits cleanly");
        assert_eq!(outcome.stdout.text(), "hello from stdin");
        assert_eq!(outcome.stdout.total, 16, "the whole input was echoed");
        assert!(!outcome.timed_out);
    }

    #[test]
    fn a_child_killed_at_the_deadline_keeps_its_partial_capped_output() {
        let workspace = Workspace::new("deadline");
        let yes = helper_path("yes").expect("`yes` exists on a supported host");
        let cap = 1024;
        let outcome = run_child(
            &yes,
            &["aaaa"],
            &workspace.0,
            &[],
            None,
            Duration::from_millis(200),
            cap,
        )
        .expect("the child starts");

        assert!(outcome.timed_out, "the runaway is killed");
        assert_eq!(outcome.stdout.head.len(), cap, "exactly the cap is kept");
        assert!(outcome.stdout.truncated, "the tail was dropped");
        assert!(
            outcome.stdout.total > cap as u64,
            "the child wrote past the cap before it was killed"
        );
    }
}
