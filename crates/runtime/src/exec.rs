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
use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use agent_core::event::ErrorClass;
use rustix::process::{Pid, Signal, kill_process_group};

use crate::{Tool, confine};

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

/// Starts the tool, writes the arguments to it, drains its output while it
/// runs, and waits no longer than its manifest allows.
#[must_use]
pub fn run(
    tool: &Tool,
    args: &str,
    working: &Path,
    heartbeat: &mut dyn FnMut(),
) -> Outcome {
    let Some((program, _)) = tool.argv.split_first() else {
        return Outcome::Failed(ErrorClass::Unknown, false);
    };
    let Some(program) = confine::resolve(program) else {
        return Outcome::Failed(ErrorClass::Unknown, false);
    };
    // A tool that the configuration bounds is started by the launcher, which puts
    // the bounds on itself and then becomes the program. Nothing else can put them
    // on a child, and a bound the child did not inherit is not a bound.
    let line = if tool.confined {
        let Ok(launcher) = confine::launcher() else {
            return Outcome::Failed(ErrorClass::Unknown, false);
        };
        confine::plan(&launcher, tool, &program, working)
    } else {
        let mut plain: Vec<std::ffi::OsString> = vec![program.into_os_string()];
        plain.extend(tool.argv.iter().skip(1).map(std::ffi::OsString::from));
        plain
    };
    let Some((launcher, bounds)) = line.split_first() else {
        return Outcome::Failed(ErrorClass::Unknown, false);
    };

    let started = Instant::now();
    let mut command = Command::new(launcher);
    command
        .current_dir(working)
        .args(bounds)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        // The group is what a timeout kills. A build starts children of its own,
        // and killing only the tool leaves them writing into a pipe nobody reads.
        .process_group(0);
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

    // The child may write before it reads, so both halves need to run at once.
    // The reader keeps one byte past the bound and drains the rest, because a
    // reader that stops at the bound leaves the child blocked on a full pipe.
    let max = tool.max_output;
    let stdout = child.stdout.take();
    let reader = std::thread::spawn(move || read_capped(stdout, max));
    let arguments = args.to_owned();
    let stdin = child.stdin.take();
    let writer = std::thread::spawn(move || {
        if let Some(mut stdin) = stdin {
            let _ = stdin.write_all(arguments.as_bytes());
        }
    });

    let Some(status) = wait(&mut child, tool.timeout, heartbeat) else {
        // The process outlived its bound. It may have done half its work, so the
        // log has to say so rather than claim it never happened.
        stop(&mut child);
        let _ = child.wait();
        return Outcome::Failed(ErrorClass::Timeout, true);
    };

    let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let output = reader.join().unwrap_or_default();
    let _ = writer.join();
    let (output, truncated) = cut(&output, tool.max_output);
    Outcome::Ran(Ran {
        duration_ms,
        output,
        status: status.code().unwrap_or(-1),
        truncated,
    })
}

/// What one stream carried, with its ends kept and its middle dropped.
#[derive(Default)]
struct Output {
    /// The first bytes, up to one past the bound, so a short stream is whole.
    raw: Vec<u8>,
    /// The last half of the bound, so a long stream keeps its ending.
    tail: Vec<u8>,
    total: u64,
}

/// Reads the child's output while it runs, keeping the bound and draining the rest.
fn read_capped(
    reader: Option<std::process::ChildStdout>,
    max: usize,
) -> Output {
    let Some(mut reader) = reader else {
        return Output::default();
    };
    let half = max / 2;
    let mut output = Output::default();
    let mut buffer = [0_u8; 8192];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                output.total = output.total.saturating_add(read as u64);
                keep_head(&mut output.raw, &buffer[..read], max);
                keep_tail(&mut output.tail, &buffer[..read], half);
            },
        }
    }
    output
}

/// Keeps the first bytes, up to one past the bound.
fn keep_head(
    raw: &mut Vec<u8>,
    chunk: &[u8],
    max: usize,
) {
    if raw.len() > max {
        return;
    }
    let room = max.saturating_add(1).saturating_sub(raw.len());
    raw.extend_from_slice(&chunk[..room.min(chunk.len())]);
}

/// Keeps the last bytes, so a long stream still ends with its own ending.
fn keep_tail(
    tail: &mut Vec<u8>,
    chunk: &[u8],
    half: usize,
) {
    if half == 0 {
        return;
    }
    if chunk.len() >= half {
        tail.clear();
        tail.extend_from_slice(&chunk[chunk.len() - half..]);
        return;
    }
    tail.extend_from_slice(chunk);
    if tail.len() > half {
        let drop = tail.len() - half;
        tail.drain(..drop);
    }
}

/// Stops the tool and the children it started.
fn stop(child: &mut std::process::Child) {
    if let Ok(raw) = i32::try_from(child.id())
        && let Some(pid) = Pid::from_raw(raw)
    {
        let _ = kill_process_group(pid, Signal::KILL);
    }
    let _ = child.kill();
}

/// Waits for the process, or gives up.
fn wait(
    child: &mut std::process::Child,
    limit: Duration,
    heartbeat: &mut dyn FnMut(),
) -> Option<std::process::ExitStatus> {
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => {},
            Err(_) => return None,
        }
        heartbeat();
        if started.elapsed() >= limit {
            return None;
        }
        std::thread::sleep(POLL);
    }
}

/// Keeps the head and the tail, which is where a program puts its summary and
/// its error, and cuts the middle out.
fn cut(
    output: &Output,
    max: usize,
) -> (String, bool) {
    if output.total <= max as u64 {
        return (String::from_utf8_lossy(&output.raw).into_owned(), false);
    }
    let half = max / 2;
    let head = String::from_utf8_lossy(&output.raw[..half.min(output.raw.len())]);
    let tail = String::from_utf8_lossy(&output.tail);
    let kept = (half + output.tail.len()) as u64;
    let dropped = output.total.saturating_sub(kept);
    (format!("{head}\n...{dropped}\n{tail}"), true)
}

#[cfg(test)]
mod tests;
