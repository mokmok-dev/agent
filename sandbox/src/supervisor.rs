//! The in-namespace init that runs the forwarder and the confined command.
//!
//! A confined command with egress needs two processes in its network namespace:
//! the command, and the [`egress-forward`](crate::egress::Forwarder) that bridges
//! the command's loopback to the proxy socket. bubblewrap executes a single
//! process as the namespace's init, so this supervisor is that process: it starts
//! the forwarder, waits for it to listen, starts the command, waits for the
//! command, stops the forwarder, and exits with the command's code.
//!
//! Because it is PID 1, the kernel reaps orphaned descendants into it, and a
//! `SIGKILL` to `bwrap` (the executor's timeout) tears the whole tree down with
//! `--die-with-parent`. See `docs/sandbox/supervisor.md`.

use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

/// Why a supervisor configuration could not be built.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SupervisorError {
    /// The `--forward` argument was not given.
    #[error("the supervisor needs --forward <path>")]
    MissingForward,
    /// The `--socket` argument was not given.
    #[error("the supervisor needs --socket <path>")]
    MissingSocket,
    /// The `--port` argument was not given.
    #[error("the supervisor needs --port <port>")]
    MissingPort,
    /// The `--port` argument was not a number in `0..=65535`.
    #[error("`{value}` is not a valid port")]
    BadPort {
        /// The rejected value.
        value: String,
    },
    /// The `--` separator or a command was not given.
    #[error("the supervisor needs `-- <program> [args...]`")]
    MissingCommand,
    /// An argument before `--` was not recognised.
    #[error("unknown argument `{argument}` before `--`")]
    UnknownArgument {
        /// The rejected argument.
        argument: String,
    },
}

/// What the supervisor should run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupervisorConfig {
    /// The `egress-forward` binary, already resolved on the trusted side.
    pub forwarder: PathBuf,
    /// The proxy socket the forwarder bridges to.
    pub socket: PathBuf,
    /// The loopback port the forwarder binds.
    pub port: u16,
    /// The confined command and its arguments.
    pub command: Vec<OsString>,
}

impl SupervisorConfig {
    /// Parse `--forward <path> --socket <path> --port <port> -- <program> [args...]`.
    ///
    /// Everything after `--` is the command, verbatim, so an argument of the
    /// command is never mistaken for a supervisor flag. The parser is deliberately
    /// tiny and dependency-free: the supervisor is std-only so it stays cheap to
    /// build and audit, and four flags do not justify a parser crate.
    ///
    /// # Errors
    ///
    /// Returns a [`SupervisorError`] for a missing, unknown, or malformed
    /// argument.
    pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Self, SupervisorError> {
        let mut forwarder = None;
        let mut socket = None;
        let mut port = None;
        let mut command = Vec::new();
        let mut args = args.into_iter();
        while let Some(argument) = args.next() {
            if argument == "--" {
                command.extend(args);
                break;
            }
            match argument.to_str() {
                Some("--forward") => {
                    forwarder = Some(PathBuf::from(
                        args.next().ok_or(SupervisorError::MissingForward)?,
                    ));
                },
                Some("--socket") => {
                    socket = Some(PathBuf::from(
                        args.next().ok_or(SupervisorError::MissingSocket)?,
                    ));
                },
                Some("--port") => {
                    let value = args.next().ok_or(SupervisorError::MissingPort)?;
                    let text = value.to_str().ok_or_else(|| SupervisorError::BadPort {
                        value: value.to_string_lossy().into_owned(),
                    })?;
                    port = Some(text.parse().map_err(|_| SupervisorError::BadPort {
                        value: text.to_owned(),
                    })?);
                },
                _ => {
                    return Err(SupervisorError::UnknownArgument {
                        argument: argument.to_string_lossy().into_owned(),
                    });
                },
            }
        }
        if command.is_empty() {
            return Err(SupervisorError::MissingCommand);
        }
        Ok(Self {
            forwarder: forwarder.ok_or(SupervisorError::MissingForward)?,
            socket: socket.ok_or(SupervisorError::MissingSocket)?,
            port: port.ok_or(SupervisorError::MissingPort)?,
            command,
        })
    }
}

/// Why the supervisor could not run.
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    /// The forwarder could not be started.
    #[error("could not start the forwarder: {0}")]
    ForwarderSpawn(#[source] std::io::Error),
    /// The forwarder exited before it began listening, so the bridge is broken
    /// and the command must not run.
    #[error("the forwarder exited before it began listening")]
    ForwarderFailed,
    /// The confined command could not be started.
    #[error("could not start the confined command: {0}")]
    CommandSpawn(#[source] std::io::Error),
    /// The confined command could not be waited on.
    #[error("could not wait for the confined command: {0}")]
    CommandWait(#[source] std::io::Error),
    /// The configuration has no command to run. [`SupervisorConfig::parse`]
    /// prevents this; it guards a hand-built config.
    #[error("no command to run")]
    NoCommand,
}

/// How a supervised run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SupervisorOutcome {
    /// The command's exit code, or `None` when a signal ended it.
    pub code: Option<i32>,
}

impl SupervisorOutcome {
    /// The process exit code the supervisor itself should return: the command's
    /// code, or `128` for a signal exit (the convention is `128 + signal`, which
    /// `std::process::ExitStatus` does not expose portably).
    #[must_use]
    pub const fn exit_code(self) -> i32 {
        match self.code {
            Some(code) => code,
            None => 128,
        }
    }
}

/// Run the forwarder and the command together, returning the command's exit.
///
/// Starts the forwarder, waits for it to report that it is listening, starts the
/// command, waits for the command, stops the forwarder, and returns the
/// command's code.
///
/// # Errors
///
/// Returns a [`RunError`] if the forwarder exits before listening (the bridge is
/// broken, so the command is not run), or if a process cannot be started or
/// waited on.
pub fn run(config: &SupervisorConfig) -> Result<SupervisorOutcome, RunError> {
    let mut forwarder = start_forwarder(config)?;
    await_forwarder_ready(&mut forwarder)?;

    let mut command = start_command(config)?;
    let status = command.wait().map_err(RunError::CommandWait)?;

    // The command has ended, so the forwarder has nothing left to bridge. Stop it
    // and reap it.
    let _ = forwarder.kill();
    let _ = forwarder.wait();

    Ok(SupervisorOutcome {
        code: status.code(),
    })
}

/// Start `egress-forward` with its stderr piped, for the readiness handshake.
fn start_forwarder(config: &SupervisorConfig) -> Result<Child, RunError> {
    Command::new(&config.forwarder)
        .arg("--socket")
        .arg(&config.socket)
        .arg("--port")
        .arg(config.port.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(RunError::ForwarderSpawn)
}

/// Wait until the forwarder reports that it is listening, or fail closed.
///
/// The forwarder prints `egress-forward: listening on 127.0.0.1:<port>` on
/// stderr. The supervisor reads that line before starting the command, so an
/// early proxy connection is not refused. If the forwarder exits first, the
/// handshake ends with EOF and the bridge is broken, so this fails rather than
/// run the command with no route out. The remaining stderr is drained on its own
/// thread, so a chatty forwarder cannot fill the pipe and block.
fn await_forwarder_ready(forwarder: &mut Child) -> Result<(), RunError> {
    let stderr = forwarder.stderr.take().ok_or(RunError::ForwarderFailed)?;
    let mut reader = BufReader::new(stderr);
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => return Err(RunError::ForwarderFailed),
            Ok(_) => {
                if line.contains("listening on") {
                    break;
                }
            },
        }
    }
    // Keep draining the pipe, or the forwarder blocks once it fills.
    std::thread::spawn(move || {
        let mut sink = Vec::new();
        let _ = reader.read_to_end(&mut sink);
    });
    Ok(())
}

/// Start the confined command.
fn start_command(config: &SupervisorConfig) -> Result<Child, RunError> {
    let (program, args) = config.command.split_first().ok_or(RunError::NoCommand)?;
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(RunError::CommandSpawn)
}

#[cfg(test)]
mod tests {
    // Tests for the argument parser and the orchestration, driven by a stub
    // "forwarder" script. These need no confinement, so they run on any host.

    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    use super::*;

    /// `parts` as the `OsString` arguments a process would receive.
    fn args(parts: &[&str]) -> Vec<OsString> {
        parts.iter().map(OsString::from).collect()
    }

    /// Write an executable stub script, returning its path.
    fn stub(
        tag: &str,
        body: &str,
    ) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("sandbox-supervisor-{tag}-{}", std::process::id()));
        std::fs::write(&path, body).expect("writes the stub");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("makes it executable");
        path
    }

    fn config(
        forwarder: PathBuf,
        command: &[&str],
    ) -> SupervisorConfig {
        SupervisorConfig {
            forwarder,
            socket: PathBuf::from("/run/egress.sock"),
            port: 8080,
            command: command.iter().map(OsString::from).collect(),
        }
    }

    /// Run `super::run`, retrying the fixture's write-then-exec race.
    ///
    /// A script written and immediately executed can hit `ETXTBSY` while another
    /// test thread's write handle is briefly still open. That is a fixture race,
    /// not supervisor behaviour, so it is retried rather than surfaced.
    fn run_retrying_busy(config: &SupervisorConfig) -> Result<SupervisorOutcome, RunError> {
        for _ in 0..20 {
            match run(config) {
                Err(RunError::ForwarderSpawn(error))
                    if error.kind() == std::io::ErrorKind::ExecutableFileBusy =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(25));
                },
                other => return other,
            }
        }
        run(config)
    }

    #[test]
    fn the_arguments_parse() {
        let parsed = SupervisorConfig::parse(args(&[
            "--forward",
            "/bin/ef",
            "--socket",
            "/run/s.sock",
            "--port",
            "8080",
            "--",
            "/bin/echo",
            "hi",
        ]))
        .expect("parses");
        assert_eq!(parsed.forwarder, PathBuf::from("/bin/ef"));
        assert_eq!(parsed.socket, PathBuf::from("/run/s.sock"));
        assert_eq!(parsed.port, 8080);
        assert_eq!(parsed.command, args(&["/bin/echo", "hi"]));
    }

    #[test]
    fn everything_after_the_separator_is_the_command() {
        // A command argument that looks like a flag must not be read as one.
        let parsed = SupervisorConfig::parse(args(&[
            "--forward",
            "/f",
            "--socket",
            "/s",
            "--port",
            "1",
            "--",
            "/bin/echo",
            "--forward",
        ]))
        .expect("parses");
        assert_eq!(parsed.command, args(&["/bin/echo", "--forward"]));
    }

    #[test]
    fn a_missing_piece_is_rejected() {
        assert_eq!(
            SupervisorConfig::parse(args(&["--socket", "/s", "--port", "1", "--", "x"])),
            Err(SupervisorError::MissingForward)
        );
        assert_eq!(
            SupervisorConfig::parse(args(&["--forward", "/f", "--port", "1", "--", "x"])),
            Err(SupervisorError::MissingSocket)
        );
        assert_eq!(
            SupervisorConfig::parse(args(&["--forward", "/f", "--socket", "/s", "--", "x"])),
            Err(SupervisorError::MissingPort)
        );
        assert_eq!(
            SupervisorConfig::parse(args(&["--forward", "/f", "--socket", "/s", "--port", "1"])),
            Err(SupervisorError::MissingCommand)
        );
        assert_eq!(
            SupervisorConfig::parse(args(&[
                "--forward",
                "/f",
                "--socket",
                "/s",
                "--port",
                "1",
                "--"
            ])),
            Err(SupervisorError::MissingCommand)
        );
    }

    #[test]
    fn a_non_numeric_port_is_rejected() {
        assert!(matches!(
            SupervisorConfig::parse(args(&[
                "--forward",
                "/f",
                "--socket",
                "/s",
                "--port",
                "x",
                "--",
                "c"
            ])),
            Err(SupervisorError::BadPort { .. })
        ));
    }

    #[test]
    fn an_unknown_flag_is_rejected() {
        assert!(matches!(
            SupervisorConfig::parse(args(&[
                "--forward",
                "/f",
                "--socket",
                "/s",
                "--port",
                "1",
                "--oops",
                "--",
                "c"
            ])),
            Err(SupervisorError::UnknownArgument { .. })
        ));
    }

    #[test]
    fn a_signal_exit_maps_to_128() {
        assert_eq!(SupervisorOutcome { code: Some(0) }.exit_code(), 0);
        assert_eq!(SupervisorOutcome { code: Some(7) }.exit_code(), 7);
        assert_eq!(SupervisorOutcome { code: None }.exit_code(), 128);
    }

    #[test]
    fn the_supervisor_runs_the_command_and_returns_its_code() {
        if !Path::new("/bin/sh").exists() {
            return;
        }
        // A stub forwarder that reports readiness and then stays alive.
        let forwarder = stub(
            "ready",
            "#!/bin/sh\necho 'egress-forward: listening on 127.0.0.1:8080' >&2\nsleep 30\n",
        );
        let config = config(forwarder.clone(), &["/bin/sh", "-c", "exit 7"]);
        let outcome = run_retrying_busy(&config).expect("runs");
        assert_eq!(outcome.code, Some(7), "the command's code is returned");
        let _ = std::fs::remove_file(&forwarder);
    }

    #[test]
    fn the_supervisor_fails_closed_when_the_forwarder_never_listens() {
        if !Path::new("/bin/sh").exists() {
            return;
        }
        // A stub forwarder that exits without reporting readiness.
        let forwarder = stub("dead", "#!/bin/sh\nexit 3\n");
        // The command would create a marker; it must not run.
        let marker =
            std::env::temp_dir().join(format!("sandbox-supervisor-marker-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        let config = config(
            forwarder.clone(),
            &["/bin/sh", "-c", &format!("touch {}", marker.display())],
        );
        let result = run_retrying_busy(&config);
        assert!(
            matches!(result, Err(RunError::ForwarderFailed)),
            "a forwarder that never listens must fail the run, got {result:?}"
        );
        assert!(!marker.exists(), "the command must not have run");
        let _ = std::fs::remove_file(&forwarder);
    }

    #[test]
    fn the_supervisor_stops_the_forwarder_when_the_command_ends() {
        if !Path::new("/bin/sh").exists() {
            return;
        }
        // The stub forwarder writes its pid, reports readiness, and sleeps. After
        // the run, its pid must be gone: the supervisor stopped it.
        let pidfile =
            std::env::temp_dir().join(format!("sandbox-supervisor-pid-{}", std::process::id()));
        let _ = std::fs::remove_file(&pidfile);
        let forwarder = stub(
            "pid",
            &format!(
                "#!/bin/sh\necho $$ > {}\necho 'egress-forward: listening on 127.0.0.1:8080' >&2\nsleep 30\n",
                pidfile.display()
            ),
        );
        let config = config(forwarder.clone(), &["/bin/sh", "-c", "exit 0"]);
        let outcome = run_retrying_busy(&config).expect("runs");
        assert_eq!(outcome.code, Some(0));

        let pid: i32 = std::fs::read_to_string(&pidfile)
            .expect("reads the pid")
            .trim()
            .parse()
            .expect("parses the pid");
        // `kill -0` succeeds only if the process exists; a stopped forwarder does
        // not. A tiny grace period covers the reap.
        std::thread::sleep(std::time::Duration::from_millis(200));
        let alive = Command::new("/bin/sh")
            .args(["-c", &format!("kill -0 {pid} 2>/dev/null")])
            .status()
            .expect("runs kill");
        assert!(!alive.success(), "the forwarder should have been stopped");
        let _ = std::fs::remove_file(&forwarder);
        let _ = std::fs::remove_file(&pidfile);
    }
}
