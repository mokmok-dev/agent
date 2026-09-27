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
use std::process::{Child, ExitStatus};
use std::time::{Duration, Instant};

use crate::filesystem::Backend;

use super::{ExecError, ExecRequest, Output, start_confined};

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

/// A running long-lived confined process.
///
/// The output pipes are inherited from the daemon's standard streams rather than
/// captured: a long-lived process is not a request whose output is returned, and
/// an uncaptured pipe cannot fill and block it. A caller that wants the output
/// redirects the child itself before spawning, or reads it from wherever the
/// backend sends it.
#[derive(Debug)]
pub struct Process {
    child: Child,
    started: Instant,
    /// Whether [`Process::kill`] ended it, so the outcome can record a stop
    /// rather than an exit on its own.
    killed: bool,
}

impl Process {
    /// Spawn `request` under `backend`.
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
    ) -> Result<Self, ExecError> {
        let request = super::with_proxy_env(request);
        let child = start_confined(backend, &request, scratch, Output::Inherited)?;

        Ok(Self {
            child,
            started: Instant::now(),
            killed: false,
        })
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

    use std::path::Path;
    use std::process::{Command, Stdio};

    use super::*;

    /// A `Process` around a plain `/bin/sh -c <script>`, bypassing the backend.
    fn shell(script: &str) -> Option<Process> {
        if !Path::new("/bin/sh").exists() {
            return None;
        }
        let child = Command::new("/bin/sh")
            .args(["-c", script])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        Some(Process {
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
