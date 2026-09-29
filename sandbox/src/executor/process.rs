//! Spawning a long-lived confined process and reporting its lifecycle.
//!
//! A one-shot command is [`run`](super::run): it starts, is waited on under a
//! deadline, and yields a terminal [`ExecOutcome`](super::ExecOutcome). A
//! long-lived process is different: it is expected to keep running, so the daemon
//! holds a handle, can observe whether it is alive, and can stop it. The handle is
//! [`Process`], and the lifecycle events are `process.started` and
//! `process.exited` (see [`crate::events`]).
//!
//! The confinement is the same [`Backend`] the one-shot executor uses, and the same
//! [`ExecRequest`], so a long-lived process is launched identically — including the
//! egress supervisor when one is granted. Its children inherit the policy, and a
//! kill reaches the whole tree: the backend runs the command as its PID
//! namespace's init, so a `SIGKILL` to it brings down every descendant.

use std::path::Path;
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use crate::filesystem::Backend;

use super::{ExecError, ExecRequest, start_confined};

/// The terminal state of a long-lived process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessOutcome {
    /// The exit code, or `None` when a signal ended it.
    pub code: Option<i32>,
    /// How long the process ran.
    pub duration: Duration,
    /// Whether [`Process::kill`] ended it, rather than it exiting on its own.
    pub killed: bool,
}

/// How a long-lived process's standard streams are wired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessStdio {
    /// Inherit the daemon's streams: input and output are not captured. The
    /// caller cannot write to the process, and a pipe it does not own cannot
    /// fill, so nothing needs draining.
    Inherited,
    /// Pipe all three streams, so the caller can write standard input and read
    /// standard output and error. This is what a caller uses to exchange data
    /// with the process.
    ///
    /// **The caller must drain standard output and standard error**, on their
    /// own threads. A full pipe blocks the process until it is read, so a caller
    /// that waits for the process without reading can deadlock. Dropping the
    /// standard input handle sends end of file to the process.
    Piped,
}

/// A running long-lived confined process.
///
/// The standard streams follow [`ProcessStdio`]. With [`ProcessStdio::Piped`] the
/// pipes are held on the handle and handed out by [`Process::take_stdin`],
/// [`Process::take_stdout`], and [`Process::take_stderr`]. A caller that wants
/// the output moves each pipe to a reader thread itself, because a read blocks
/// until data arrives or the pipe closes, and this handle must stay usable for
/// [`Process::is_running`], [`Process::kill`], and [`Process::wait`] meanwhile.
#[derive(Debug)]
pub struct Process {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: Option<ChildStdout>,
    stderr: Option<ChildStderr>,
    started: Instant,
    /// Whether [`Process::kill`] ended it, so the outcome can record a stop
    /// rather than an exit on its own.
    killed: bool,
}

impl Process {
    /// Spawn `request` under `backend` with `stdio`.
    ///
    /// The request is the same [`ExecRequest`] a one-shot command uses, so a
    /// long-lived process with an egress grant is launched through the same
    /// supervisor and proxy injection.
    ///
    /// # Errors
    ///
    /// Returns [`ExecError::NoBackend`] when `backend` cannot confine a command,
    /// [`ExecError::Render`] when the policy cannot be rendered, or
    /// [`ExecError::Spawn`] when the backend cannot be started.
    pub fn spawn(
        backend: &Backend,
        request: &ExecRequest,
        scratch: &Path,
        stdio: ProcessStdio,
    ) -> Result<Self, ExecError> {
        let request = super::with_proxy_env(request);
        let (child_stdin, child_stdout, child_stderr) = match stdio {
            ProcessStdio::Inherited => (Stdio::inherit(), Stdio::inherit(), Stdio::inherit()),
            ProcessStdio::Piped => (Stdio::piped(), Stdio::piped(), Stdio::piped()),
        };
        let mut child = start_confined(
            backend,
            &request,
            scratch,
            child_stdin,
            child_stdout,
            child_stderr,
        )?;

        Ok(Self {
            stdin: child.stdin.take(),
            stdout: child.stdout.take(),
            stderr: child.stderr.take(),
            child,
            started: Instant::now(),
            killed: false,
        })
    }

    /// Take the standard input pipe, for writing, or `None` when the process was
    /// not spawned with [`ProcessStdio::Piped`] or the pipe was already taken.
    ///
    /// Dropping the returned handle closes the pipe and sends end of file to the
    /// process.
    #[must_use]
    pub const fn take_stdin(&mut self) -> Option<ChildStdin> {
        self.stdin.take()
    }

    /// Take the standard output pipe, for reading. See [`Process::take_stdin`].
    #[must_use]
    pub const fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.stdout.take()
    }

    /// Take the standard error pipe, for reading. See [`Process::take_stdin`].
    #[must_use]
    pub const fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.stderr.take()
    }

    /// The process's id, for logs and for a caller that signals it directly.
    #[must_use]
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// How long the process has been running.
    #[must_use]
    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// Whether the process is still running.
    ///
    /// # Errors
    ///
    /// Returns the underlying [`std::io::Error`] if the wait cannot be performed.
    pub fn is_running(&mut self) -> std::io::Result<bool> {
        Ok(self.child.try_wait()?.is_none())
    }

    /// Stop the process and its descendants.
    ///
    /// The kill is a `SIGKILL` to the backend, which runs the command as its PID
    /// namespace's init; the kernel then kills every process in that namespace, so
    /// descendants cannot survive. The process is marked killed, so its
    /// [`ProcessOutcome`] records that it was stopped rather than exiting on its
    /// own.
    ///
    /// # Errors
    ///
    /// Returns the underlying [`std::io::Error`] if the kill fails.
    pub fn kill(&mut self) -> std::io::Result<()> {
        self.killed = true;
        self.child.kill()
    }

    /// Wait for the process to end, reporting its terminal state.
    ///
    /// # Errors
    ///
    /// Returns the underlying [`std::io::Error`] if the wait fails.
    pub fn wait(&mut self) -> std::io::Result<ProcessOutcome> {
        let status: ExitStatus = self.child.wait()?;
        Ok(ProcessOutcome {
            code: status.code(),
            duration: self.started.elapsed(),
            killed: self.killed,
        })
    }

    /// Whether [`kill`](Self::kill) was called.
    #[must_use]
    pub const fn was_killed(&self) -> bool {
        self.killed
    }
}

#[cfg(test)]
mod tests {
    // Tests for the long-lived handle that need no confinement: spawn a plain
    // child (not a confined one) so they run on a host that cannot build a
    // namespace, matching the rule for the executor's timeout tests.

    use std::io::Read;
    use std::path::Path;
    use std::process::{Command, Stdio};

    use super::*;

    /// A `Process` around a plain `/bin/sh -c <script>`, bypassing the backend.
    ///
    /// Built with all three streams piped, so a test can exercise the pipe
    /// accessors without confinement. A test that does not read them drops them.
    fn shell(script: &str) -> Option<Process> {
        if !Path::new("/bin/sh").exists() {
            return None;
        }
        let mut child = Command::new("/bin/sh")
            .args(["-c", script])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .ok()?;
        Some(Process {
            stdin: child.stdin.take(),
            stdout: child.stdout.take(),
            stderr: child.stderr.take(),
            child,
            started: Instant::now(),
            killed: false,
        })
    }

    #[test]
    fn a_short_lived_process_reports_its_exit_code() {
        let Some(mut process) = shell("exit 7") else {
            return;
        };
        let outcome = process.wait().expect("waits");
        assert_eq!(outcome.code, Some(7));
        assert!(!outcome.killed, "it exited on its own");
    }

    #[test]
    fn a_running_process_reports_that_it_is_alive() {
        let Some(mut process) = shell("sleep 30") else {
            return;
        };
        assert!(process.is_running().expect("checks"), "sleep 30 is running");
        assert!(!process.was_killed(), "nothing killed it yet");
        process.kill().expect("kills");
        // `kill` marks the process before signalling, so this holds without
        // waiting on the child. A `kill` that did nothing would leave it false,
        // and is caught here rather than by blocking on `sleep 30`.
        assert!(process.was_killed(), "kill marks the process");
        let outcome = process.wait().expect("waits");
        assert!(outcome.killed, "a killed process is marked killed");
    }

    #[test]
    fn a_killed_process_is_not_reported_as_a_clean_exit() {
        let Some(mut process) = shell("sleep 30") else {
            return;
        };
        process.kill().expect("kills");
        assert!(process.was_killed(), "kill marks the process");
        let outcome = process.wait().expect("waits");
        assert_ne!(outcome.code, Some(0), "a signal exit has no zero code");
        assert!(outcome.killed);
    }

    #[test]
    fn a_finished_process_is_not_running() {
        let Some(mut process) = shell("exit 0") else {
            return;
        };
        let _ = process.wait().expect("waits");
        assert!(
            !process.is_running().expect("checks"),
            "a process that exited is not running"
        );
    }

    #[test]
    fn the_pid_is_a_real_process_id_not_one() {
        // A spawned child's pid is never 1 (the init process), so this rules out
        // a constant stub while staying true on every host.
        let Some(process) = shell("sleep 30") else {
            return;
        };
        assert!(process.pid() > 1, "the pid is the child's, not a constant");
        drop(process);
    }

    #[test]
    fn piped_stdin_round_trips_through_the_process() {
        // The process copies its standard input to its standard output, so a
        // value written to the handle must come back on the output pipe. A
        // handle that wrote nowhere, or an output pipe the child never owned,
        // fails this.
        let Some(mut process) = shell("cat") else {
            return;
        };
        let mut stdin = process.take_stdin().expect("stdin is piped");
        let mut stdout = process.take_stdout().expect("stdout is piped");

        // Drain on a thread, because the read blocks until the write closes the
        // pipe, and the write happens on this thread.
        let reader = std::thread::spawn(move || {
            let mut buffer = String::new();
            stdout.read_to_string(&mut buffer).expect("reads");
            buffer
        });

        std::io::Write::write_all(&mut stdin, b"hello\n").expect("writes");
        drop(stdin);

        let echoed = reader.join().expect("joins");
        assert_eq!(echoed, "hello\n", "the child echoes what it read");
    }

    #[test]
    fn inherited_stdio_exposes_no_pipes() {
        // The accessors report absence rather than a pipe the process does not
        // have, so a caller cannot try to read a stream that was inherited.
        if !Path::new("/bin/sh").exists() {
            return;
        }
        let mut child = Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawns");
        let mut process = Process {
            stdin: child.stdin.take(),
            stdout: child.stdout.take(),
            stderr: child.stderr.take(),
            child,
            started: Instant::now(),
            killed: false,
        };
        assert!(process.take_stdin().is_none());
        assert!(process.take_stdout().is_none());
        assert!(process.take_stderr().is_none());
    }

    #[test]
    fn elapsed_measures_the_time_since_spawn() {
        let Some(process) = shell("sleep 30") else {
            return;
        };
        // Let real time pass, then require it to be reflected. A defaulted (zero)
        // elapsed would fail.
        std::thread::sleep(Duration::from_millis(20));
        assert!(
            process.elapsed() >= Duration::from_millis(10),
            "elapsed should reflect the time since spawn"
        );
        drop(process);
    }
}
