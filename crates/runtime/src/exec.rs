//! Running one tool call.
//!
//! The effect happens here and nowhere else: a process starts, writes its
//! output, and dies. What the log records is decided in [`super::Reactor`], not
//! here, so this module can be read as the answer to "what did the machine do?"
//! and nothing more.
//!
//! The arguments the model wrote arrive on the tool's standard input, as the
//! bytes it wrote. Nothing in this build parses them.

use std::io::{Read as _, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use agent_core::event::ErrorClass;

use crate::Tool;

/// How long to wait before asking a running process again whether it is done.
const POLL: Duration = Duration::from_millis(5);

/// What came of one call.
pub enum Outcome {
    /// The process ran and stopped, whatever it returned.
    Ran(Ran),
    /// The process did not produce a result. The flag says whether the effect
    /// may have happened anyway, which the log has to record.
    Failed(ErrorClass, bool),
}

/// One finished run.
pub struct Ran {
    pub duration_ms: u64,
    pub output: String,
    pub status: i32,
    pub truncated: bool,
}

/// Starts the tool, writes the arguments to it, and waits no longer than its
/// manifest allows.
#[must_use]
pub fn run(
    tool: &Tool,
    args: &str,
    working: &Path,
) -> Outcome {
    let Some((program, rest)) = tool.argv.split_first() else {
        return Outcome::Failed(ErrorClass::Unknown, false);
    };
    let Some(program) = resolve(program) else {
        return Outcome::Failed(ErrorClass::Unknown, false);
    };
    let started = Instant::now();
    let mut command = Command::new(&program);
    command
        .args(rest)
        .current_dir(working)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // Inheriting the daemon's environment would let any tool print a credential
    // into a log that is append-only and copied by fork.
    command.env_clear();
    for name in &tool.env {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    let Ok(mut child) = command.spawn() else {
        return Outcome::Failed(ErrorClass::Unknown, false);
    };

    // Writing before reading is safe here because the tool reads its arguments
    // first: a tool that writes its whole output before reading would deadlock,
    // and its own test has to prove otherwise.
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(args.as_bytes());
    }

    let Some(status) = wait(&mut child, tool.timeout) else {
        // The process outlived its bound. It may have done half its work, so the
        // log has to say so rather than claim it never happened.
        let _ = child.kill();
        let _ = child.wait();
        return Outcome::Failed(ErrorClass::Timeout, true);
    };

    let mut raw = Vec::new();
    if let Some(mut stdout) = child.stdout.take() {
        // A tool that ignores its bound should not be able to fill memory, so the
        // read stops one byte past what the event could carry.
        let mut buffer = [0_u8; 8192];
        while raw.len() <= tool.max_output {
            match stdout.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(read) => raw.extend_from_slice(&buffer[..read]),
            }
        }
    }

    let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let (output, truncated) = cut(&raw, tool.max_output);
    Outcome::Ran(Ran {
        duration_ms,
        output,
        status: status.code().unwrap_or(-1),
        truncated,
    })
}

/// The program the manifest's `argv` names.
///
/// The child is given only the names its manifest declares, and a program named
/// without a path is found through `PATH`, so the lookup has to happen here,
/// against the daemon's environment, or a tool that declares no environment
/// could not start at all. Which program runs is the manifest's statement about
/// `argv`; what it sees is its statement about `permissions`, and one must not
/// decide the other.
fn resolve(program: &str) -> Option<PathBuf> {
    if program.contains('/') {
        return Some(PathBuf::from(program));
    }
    search(program, &std::env::var_os("PATH")?)
}

fn search(
    program: &str,
    path: &std::ffi::OsStr,
) -> Option<PathBuf> {
    std::env::split_paths(path)
        .map(|directory| directory.join(program))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && (meta.permissions().mode() & 0o111) != 0)
}

/// Waits for the process, or gives up.
fn wait(
    child: &mut std::process::Child,
    limit: Duration,
) -> Option<std::process::ExitStatus> {
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => {},
            Err(_) => return None,
        }
        if started.elapsed() >= limit {
            return None;
        }
        std::thread::sleep(POLL);
    }
}

/// Keeps the head and the tail, which is where a program puts its summary and
/// its error, and cuts the middle out.
fn cut(
    raw: &[u8],
    max: usize,
) -> (String, bool) {
    if raw.len() <= max {
        return (String::from_utf8_lossy(raw).into_owned(), false);
    }
    let half = max / 2;
    let head = String::from_utf8_lossy(&raw[..half]);
    let tail = String::from_utf8_lossy(&raw[raw.len() - half..]);
    (format!("{head}\n...{}\n{tail}", raw.len() - 2 * half), true)
}

#[cfg(test)]
mod tests;
