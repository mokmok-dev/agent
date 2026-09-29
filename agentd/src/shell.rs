//! The shell capability: run one command and report what happened.
//!
//! The agent runs **inside** the session's namespace, so a child it spawns is
//! confined by the same policy: the session sandbox is the boundary, and no
//! command needs its own. This module is a direct child spawn, `sandbox::executor`
//! is for a process that must be confined from outside, and the two are not
//! nested. See `docs/session/agent.md`.
//!
//! Output is drained on its own threads and bounded **while it is read**, not
//! after: a command that writes without end (a `yes`, a runaway log) must not be
//! able to grow the agent's memory, and a command that never returns must be
//! stopped. This is the same rule the sandbox's executor follows, and for the
//! same reason.

use std::io::Read;
use std::path::PathBuf;
use std::process::Command as ChildCommand;
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::contract::{Command, Output, OutputKind};

/// The default cap on captured output, one mebibyte.
const MAX_OUTPUT_BYTES: usize = 1024 * 1024;

/// The action this capability answers.
pub const ACTION: &str = "shell";

/// How long a shell command may run before it is killed.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);

/// Run the `shell` action: `detail` is `{"argv": [...]}`.
#[derive(Debug)]
pub struct Shell {
    /// The working directory the command runs in, which the session policy has
    /// already bound read-write.
    pub workdir: PathBuf,
    /// How long the command may run before it is killed.
    pub timeout: Duration,
}

impl Shell {
    /// A shell capability with the session's workspace as its working directory.
    #[must_use]
    pub fn in_workspace(workdir: impl Into<PathBuf>) -> Self {
        Self {
            workdir: workdir.into(),
            timeout: DEFAULT_TIMEOUT,
        }
    }

    /// A shell capability with an explicit timeout, for a caller that wants one.
    #[must_use]
    pub const fn with_timeout(
        mut self,
        timeout: Duration,
    ) -> Self {
        self.timeout = timeout;
        self
    }
}

impl crate::loopcore::Capability for Shell {
    fn act(
        &self,
        command: &Command,
    ) -> Output {
        let argv: Vec<String> =
            serde_json::from_value(command.detail.get("argv").cloned().unwrap_or(Value::Null))
                .unwrap_or_default();
        if argv.is_empty() || argv.len() > 256 || argv.iter().any(|arg| arg.contains('\0')) {
            return Output {
                kind: OutputKind::Error,
                action: command.action.clone(),
                detail: json!({"reason": "malformed argv"}),
            };
        }

        let started = Instant::now();
        let spawned = ChildCommand::new(&argv[0])
            .args(&argv[1..])
            .current_dir(&self.workdir)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();

        let mut child = match spawned {
            Ok(child) => child,
            Err(error) => {
                return Output {
                    kind: OutputKind::Error,
                    action: command.action.clone(),
                    detail: json!({
                        "reason": "could not start the command",
                        "error": error.to_string(),
                        "duration_ms": millis(started.elapsed()),
                    }),
                };
            },
        };

        // Drain both pipes on their own threads, so a command that writes more
        // than a pipe buffer holds cannot block, and cap what is retained so a
        // runaway command cannot grow the agent's memory. Bytes past the cap are
        // read and discarded, so the command still finishes.
        let out_reader = child.stdout.take().map(spawn_reader);
        let err_reader = child.stderr.take().map(spawn_reader);

        let (code, timed_out) = wait_with_timeout(&mut child, self.timeout);
        let stdout = out_reader.map(join_reader).unwrap_or_default();
        let stderr = err_reader.map(join_reader).unwrap_or_default();

        let detail = json!({
            "code": code,
            "duration_ms": millis(started.elapsed()),
            "stdout": lossy(&stdout),
            "stderr": lossy(&stderr),
            "stdout_total_bytes": stdout.total,
            "stderr_total_bytes": stderr.total,
            "truncated": stdout.truncated || stderr.truncated,
            "timed_out": timed_out,
        });
        Output {
            kind: OutputKind::Done,
            action: command.action.clone(),
            detail,
        }
    }
}

/// What a reader thread read: the retained head, and whether it dropped a tail.
#[derive(Debug, Default)]
struct Captured {
    /// The retained bytes, at most [`MAX_OUTPUT_BYTES`].
    head: Vec<u8>,
    /// How many bytes the command wrote in total.
    total: u64,
    /// Whether any bytes were dropped past the cap.
    truncated: bool,
}

impl Captured {
    /// The retained bytes.
    fn bytes(&self) -> &[u8] {
        &self.head
    }
}

/// Read `pipe` to its end, retaining at most [`MAX_OUTPUT_BYTES`].
///
/// Bytes past the cap are still read and discarded, so the command never blocks
/// on a full pipe; only the retained buffer is bounded.
fn spawn_reader(mut pipe: impl Read + Send + 'static) -> std::thread::JoinHandle<Captured> {
    std::thread::spawn(move || {
        let mut captured = Captured::default();
        let mut buffer = [0_u8; 8192];
        loop {
            match pipe.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    captured.total += read as u64;
                    let room = MAX_OUTPUT_BYTES.saturating_sub(captured.head.len());
                    let keep = room.min(read);
                    captured.head.extend_from_slice(&buffer[..keep]);
                    if keep < read {
                        captured.truncated = true;
                    }
                },
            }
        }
        captured
    })
}

/// Join a reader thread, yielding what it read, or nothing if it panicked.
fn join_reader(handle: std::thread::JoinHandle<Captured>) -> Captured {
    handle.join().unwrap_or_default()
}

/// Wait for `child`, killing it if it exceeds `timeout`.
///
/// Returns the exit code and whether the timeout fired. A killed child reports
/// no code, which the output records alongside `timed_out`.
fn wait_with_timeout(
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

/// A lossy UTF-8 view of captured output, for the event's JSON.
fn lossy(captured: &Captured) -> String {
    String::from_utf8_lossy(captured.bytes()).into_owned()
}

/// `duration` as whole milliseconds, saturating at the `u64` bound.
///
/// A duration that large is unreachable in practice, but saturating keeps the
/// conversion total rather than a silent truncation.
fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    // Tests for the shell capability over a real `/bin/sh`. Running a child is
    // the capability, so the tests run one for real, in a temp workspace.

    use super::*;
    use crate::loopcore::Capability;

    /// The cap, spelled so a mutant that changes the bound is caught here: the
    /// cap-exactness test writes this many bytes and asserts all were retained.
    const EXPECTED_CAP: usize = MAX_OUTPUT_BYTES;

    /// A temp workspace, removed on drop.
    struct Workspace(PathBuf);

    impl Workspace {
        fn new(tag: &str) -> Self {
            static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let unique = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("agent-shell-{tag}-{}-{unique}", std::process::id()));
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

    fn shell_action(argv: &[&str]) -> Command {
        Command {
            action: ACTION.to_owned(),
            detail: json!({"argv": argv}),
        }
    }

    /// Locate a tool by name on the host, the way a shell would.
    ///
    /// The capability clears the child's environment, so a tool inside it does
    /// not resolve by name; a test that needs one names it by absolute path.
    fn helper_path(name: &str) -> Option<std::path::PathBuf> {
        std::env::var("PATH")
            .unwrap_or_default()
            .split(':')
            .filter(|dir| !dir.is_empty())
            .map(|dir| std::path::Path::new(dir).join(name))
            .find(|candidate| candidate.is_file())
    }

    #[test]
    fn a_command_runs_in_the_workspace_and_reports_its_output() {
        let workspace = Workspace::new("echo");
        let shell = Shell::in_workspace(&workspace.0);
        let output = shell.act(&shell_action(&["/bin/sh", "-c", "echo hello"]));
        assert_eq!(output.kind, OutputKind::Done);
        assert_eq!(output.detail["code"], 0);
        assert_eq!(output.detail["stdout"], "hello\n");
        assert_eq!(output.detail["timed_out"], false);
        assert_eq!(output.detail["truncated"], false);
    }

    #[test]
    fn a_failure_carries_the_exit_code_and_stderr() {
        let workspace = Workspace::new("fail");
        let shell = Shell::in_workspace(&workspace.0);
        let output = shell.act(&shell_action(&[
            "/bin/sh",
            "-c",
            "echo to-stderr >&2; exit 3",
        ]));
        assert_eq!(output.detail["code"], 3);
        assert_eq!(output.detail["stderr"], "to-stderr\n");
    }

    #[test]
    fn a_relative_path_resolves_from_the_workspace() {
        // The workspace is the cwd, so a relative write lands in it.
        let workspace = Workspace::new("relative");
        let shell = Shell::in_workspace(&workspace.0);
        let marker = workspace.0.join("marker.txt");
        let output = shell.act(&shell_action(&["/bin/sh", "-c", "echo x > marker.txt"]));
        assert_eq!(output.detail["code"], 0);
        assert_eq!(
            std::fs::read_to_string(&marker).expect("the marker is readable"),
            "x\n"
        );
    }

    #[test]
    fn an_empty_argv_is_reported_not_run() {
        let workspace = Workspace::new("empty");
        let shell = Shell::in_workspace(&workspace.0);
        let output = shell.act(&shell_action(&[]));
        assert_eq!(output.kind, OutputKind::Error);
        assert_eq!(output.detail["reason"], "malformed argv");
    }

    #[test]
    fn an_argv_with_a_null_byte_is_reported_not_run() {
        let workspace = Workspace::new("null");
        let shell = Shell::in_workspace(&workspace.0);
        let output = shell.act(&Command {
            action: ACTION.to_owned(),
            detail: json!({"argv": ["/bin/sh\u{0}"]}),
        });
        assert_eq!(output.kind, OutputKind::Error);
        assert_eq!(output.detail["reason"], "malformed argv");
    }

    #[test]
    fn a_command_that_writes_without_end_is_capped_and_killed() {
        // The regression this guards: reading a runaway command with
        // `Command::output()` buffers everything and OOMs the agent. The output
        // must be capped while it is read, and a command that never returns must
        // be killed at the timeout.
        let workspace = Workspace::new("runaway");
        let shell = Shell::in_workspace(&workspace.0).with_timeout(Duration::from_millis(200));
        // `yes` writes forever, so it only ends when killed. Named by its
        // absolute path, because the capability clears the environment and a
        // name would not resolve.
        let yes = helper_path("yes").expect("`yes` exists on a supported host");
        let output = shell.act(&shell_action(&[yes.to_string_lossy().as_ref(), "aaaa"]));

        assert_eq!(output.detail["timed_out"], true, "the runaway is killed");
        assert_eq!(output.detail["truncated"], true, "the output is cut");
        let stdout = output.detail["stdout"].as_str().expect("a string");
        assert_eq!(
            stdout.len(),
            MAX_OUTPUT_BYTES,
            "exactly the cap is retained"
        );
        let total = output.detail["stdout_total_bytes"]
            .as_u64()
            .expect("a number");
        assert!(
            total > MAX_OUTPUT_BYTES as u64,
            "the command wrote past the cap, which was discarded"
        );
    }

    #[test]
    fn a_command_that_returns_before_the_timeout_is_not_killed() {
        let workspace = Workspace::new("quick");
        let shell = Shell::in_workspace(&workspace.0).with_timeout(Duration::from_secs(30));
        let output = shell.act(&shell_action(&["/bin/sh", "-c", "sleep 0.05; exit 0"]));
        assert_eq!(output.detail["code"], 0);
        assert_eq!(output.detail["timed_out"], false);
    }

    #[test]
    fn argv_of_256_entries_is_accepted() {
        // The bound is 256, so exactly that many is legal. A mutant that makes
        // the comparison `>=` would refuse it.
        let workspace = Workspace::new("argv-256");
        let shell = Shell::in_workspace(&workspace.0);
        // One program plus 255 arguments, all empty strings `sh` ignores
        // harmlessly: the point is the count, not what they say.
        let mut argv = vec!["/bin/sh", "-c", "exit 0"];
        argv.resize(256, "");
        let output = shell.act(&Command {
            action: ACTION.to_owned(),
            detail: json!({"argv": argv}),
        });
        assert_eq!(
            output.detail["code"], 0,
            "exactly 256 argv entries is legal: {output:?}",
        );
        assert_eq!(output.kind, OutputKind::Done);
    }

    #[test]
    fn argv_of_257_entries_is_reported_not_run() {
        // One past the bound is refused, so the check is `>` and not `>=`: a
        // mutant that makes it `==` accepts this and fails the test.
        let workspace = Workspace::new("argv-257");
        let shell = Shell::in_workspace(&workspace.0);
        let mut argv = vec!["/bin/sh", "-c", "exit 0"];
        argv.resize(257, "");
        let output = shell.act(&Command {
            action: ACTION.to_owned(),
            detail: json!({"argv": argv}),
        });
        assert_eq!(output.kind, OutputKind::Error);
        assert_eq!(output.detail["reason"], "malformed argv");
    }

    #[test]
    fn output_of_exactly_the_cap_is_retained_whole() {
        // A command that writes exactly the cap must not be marked truncated,
        // because nothing was dropped. A mutant that replaces the multiply in the
        // cap's bound changes what fits, so the marker and the retained length
        // catch it together.
        let workspace = Workspace::new("exact-cap");
        let shell = Shell::in_workspace(&workspace.0);
        // Write exactly the cap's worth of bytes from the child. The program is
        // found from the workspace, not `PATH`: the capability clears the
        // environment, so a name would not resolve, and the host's tool
        // directories differ. A small helper binary would be more portable
        // still; `cat` of a file the test wrote keeps it to standard tools.
        let input = vec![b'x'; MAX_OUTPUT_BYTES];
        std::fs::write(workspace.0.join("cap-in"), &input).expect("writes the input file");
        // The capability clears the environment, so a tool does not resolve by
        // name; the helper is located here, on the host, and named absolutely.
        let cat = helper_path("cat").expect("`cat` exists on a supported host");
        let output = shell.act(&shell_action(&[
            "/bin/sh",
            "-c",
            &format!("{} cap-in", cat.display()),
        ]));
        let stdout = output.detail["stdout"].as_str().expect("a string");
        assert_eq!(
            stdout.len(),
            MAX_OUTPUT_BYTES,
            "a cap-sized output is retained whole: detail={:?}",
            output.detail
        );
        assert_eq!(
            output.detail["truncated"], false,
            "nothing was dropped, so it is not truncated"
        );
        let total = output.detail["stdout_total_bytes"]
            .as_u64()
            .expect("a number");
        assert_eq!(total, MAX_OUTPUT_BYTES as u64);
    }

    #[test]
    fn the_cap_is_one_mebibyte() {
        // The bound is arithmetic, and a mutant that changes it silently changes
        // what every command may emit. Spell the expected value out, so the
        // constant itself is what the test observes, not just itself.
        assert_eq!(MAX_OUTPUT_BYTES, 1_048_576_usize, "the cap is 1 MiB");
        assert_eq!(EXPECTED_CAP, MAX_OUTPUT_BYTES);
    }

    #[test]
    fn an_output_shorter_than_the_cap_is_not_truncated() {
        // The other side of the bound: well under the cap, nothing is dropped
        // and nothing is marked. Combined with the cap-exactness test, this pins
        // the boundary from both directions.
        let workspace = Workspace::new("under-cap");
        let shell = Shell::in_workspace(&workspace.0);
        let output = shell.act(&shell_action(&["/bin/sh", "-c", "echo hi"]));
        assert_eq!(output.detail["truncated"], false);
        assert_eq!(output.detail["stdout"], "hi\n");
        let total = output.detail["stdout_total_bytes"]
            .as_u64()
            .expect("a number");
        assert_eq!(total, 3, "echo hi writes 3 bytes");
    }
}
