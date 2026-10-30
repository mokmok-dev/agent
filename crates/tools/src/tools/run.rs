//! Running one allowed program.

use std::io::{Read, Write as _};
use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use agent_core::event::Risk;
use rustix::process::{Pid, Signal, kill_process_group};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ctx::Ctx;
use crate::envelope::json_len;
use crate::error::ToolError;
use crate::{Class, Limits, Scope, Tool};

/// The most of each stream one answer carries.
const STREAM_BYTES: usize = 24 * 1024;

const POLL: Duration = Duration::from_millis(10);

/// Runs a program from the invocation's allowlist.
#[derive(Debug, Clone, Copy)]
pub struct Run;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Args {
    /// The program's name or absolute path. It must be on the allowlist.
    pub program: String,
    /// Its arguments, each passed as one word.
    #[serde(rename = "args")]
    pub arguments: Option<Vec<String>>,
    /// What to write to its standard input.
    pub stdin: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Output {
    /// The exit code, or 128 plus the signal that ended it.
    pub status: i32,
    pub timed_out: bool,
    pub stdout: String,
    pub stderr: String,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
}

impl Tool for Run {
    type Args = Args;
    type Output = Output;

    const NAME: &'static str = "run";
    const DESCRIPTION: &'static str = "Run a program from the allowlist in the workspace with a \
        structured argument list. There is no shell, so a word is passed as written. Returns the \
        exit status, standard output, and standard error.";
    const CLASS: Class = Class::NonIdempotent;
    const RISK: Risk = Risk::Write;
    const SCOPE: Scope = Scope::WorkspaceAndScratch;
    const LIMITS: Limits = Limits {
        timeout_ms: 900_000,
        event_bytes: 64 * 1024,
    };
    const ENV: &'static [&'static str] = &["PATH", "HOME", "TMPDIR", "LANG"];

    fn run(
        args: Args,
        ctx: &Ctx<'_>,
    ) -> Result<Output, ToolError> {
        execute(args, ctx, Self::LIMITS.timeout())
    }
}

pub fn execute(
    args: Args,
    ctx: &Ctx<'_>,
    limit: Duration,
) -> Result<Output, ToolError> {
    let program = ctx.allow().resolve(&args.program, ctx.value("PATH"))?;
    let mut command = Command::new(&program);
    command
        .args(args.arguments.unwrap_or_default())
        .current_dir(ctx.workspace().root())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .env_clear();
    for name in Run::ENV {
        if let Some(value) = ctx.value(name) {
            command.env(name, value);
        }
    }
    let mut child = command
        .spawn()
        .map_err(|error| ToolError::exec(format!("cannot start {}: {error}", args.program)))?;

    let out = child.stdout.take().map(drain);
    let err = child.stderr.take().map(drain);
    let mut sink = child.stdin.take();
    let input = args.stdin.map(|text| {
        std::thread::spawn(move || {
            if let Some(pipe) = sink.as_mut() {
                let _ = pipe.write_all(text.as_bytes());
            }
        })
    });

    let started = Instant::now();
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {},
            Err(_) => break None,
        }
        if started.elapsed() >= limit {
            timed_out = true;
            break None;
        }
        std::thread::sleep(POLL);
    };
    if status.is_none() {
        stop(&mut child);
    }
    let out = out
        .and_then(|handle| handle.join().ok())
        .unwrap_or_default();
    let err = err
        .and_then(|handle| handle.join().ok())
        .unwrap_or_default();
    if let Some(input) = input {
        let _ = input.join();
    }

    let status = status.map_or(-1, |status| {
        status
            .code()
            .or_else(|| status.signal().map(|signal| 128 + signal))
            .unwrap_or(-1)
    });
    let mut output = Output {
        status,
        timed_out,
        stdout: String::from_utf8_lossy(&out.0).into_owned(),
        stderr: String::from_utf8_lossy(&err.0).into_owned(),
        stdout_truncated: out.1,
        stderr_truncated: err.1,
    };
    shrink(&mut output);
    Ok(output)
}

fn drain(stream: impl Read + Send + 'static) -> std::thread::JoinHandle<(Vec<u8>, bool)> {
    std::thread::spawn(move || {
        let mut kept = Vec::new();
        let mut truncated = false;
        let mut buffer = [0_u8; 8192];
        let mut stream = stream;
        loop {
            match stream.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    let room = STREAM_BYTES.saturating_sub(kept.len());
                    let keep = room.min(read);
                    kept.extend_from_slice(&buffer[..keep]);
                    truncated |= keep < read;
                },
            }
        }
        (kept, truncated)
    })
}

fn stop(child: &mut Child) {
    if let Ok(raw) = i32::try_from(child.id())
        && let Some(pid) = Pid::from_raw(raw)
    {
        let _ = kill_process_group(pid, Signal::KILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Cuts the larger stream in half until the serialized answer fits its cap.
///
/// Each pass at least halves one stream, and a stream is no larger than the cap,
/// so the passes are bounded by the bits in one length: a wrong step ends the
/// walk instead of never reaching it.
pub(super) fn shrink(output: &mut Output) {
    for _ in 0..=usize::BITS {
        if json_len(output) <= Run::LIMITS.event_bytes {
            return;
        }
        if output.stdout.is_empty() && output.stderr.is_empty() {
            return;
        }
        if output.stdout.len() >= output.stderr.len() {
            let keep = output.stdout.len() / 2;
            output.stdout.truncate(boundary(&output.stdout, keep));
            output.stdout_truncated = true;
        } else {
            let keep = output.stderr.len() / 2;
            output.stderr.truncate(boundary(&output.stderr, keep));
            output.stderr_truncated = true;
        }
    }
}

/// The nearest character boundary at or below `at`.
pub(super) fn boundary(
    text: &str,
    at: usize,
) -> usize {
    let mut at = at.min(text.len());
    for _ in 0..=text.len() {
        if text.is_char_boundary(at) {
            return at;
        }
        at = at.saturating_sub(1);
    }
    0
}
