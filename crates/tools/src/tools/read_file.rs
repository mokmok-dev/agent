//! Reading a window of a text file.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use agent_core::event::Risk;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ctx::Ctx;
use crate::envelope::fit;
use crate::error::ToolError;
use crate::{Class, Limits, Scope, Tool};

/// The most content one call returns.
const CONTENT_BYTES: usize = 48 * 1024;

/// Reads a file in the workspace.
#[derive(Debug, Clone, Copy)]
pub struct ReadFile;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Args {
    /// The file, relative to the workspace.
    pub path: String,
    /// The first line to return, counted from one.
    pub start_line: Option<usize>,
    /// The last line to return, counted from one and included.
    pub end_line: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct Output {
    pub path: String,
    pub start_line: usize,
    /// The last line in `content`, or the start line when `content` is empty.
    pub end_line: usize,
    pub lines: usize,
    pub content: String,
    /// Whether the file holds more than this answer returned.
    pub truncated: bool,
}

impl Tool for ReadFile {
    type Args = Args;
    type Output = Output;

    const NAME: &'static str = "read_file";
    const DESCRIPTION: &'static str = "Read a UTF-8 text file in the workspace. Returns a window \
        of numbered lines and whether more of the file follows; page a long file with start_line \
        and end_line.";
    const CLASS: Class = Class::Idempotent;
    const RISK: Risk = Risk::Read;
    const SCOPE: Scope = Scope::Read;
    const LIMITS: Limits = Limits {
        timeout_ms: 10_000,
        event_bytes: 64 * 1024,
    };
    const ENV: &'static [&'static str] = &[];

    fn run(
        args: Args,
        ctx: &Ctx<'_>,
    ) -> Result<Output, ToolError> {
        let path = ctx.workspace().existing(&args.path)?;
        let metadata = std::fs::metadata(&path).map_err(|error| ToolError::io(&path, &error))?;
        if !metadata.is_file() {
            return Err(ToolError::not_a_file(format!(
                "{} is not a file",
                args.path
            )));
        }
        let start = args.start_line.unwrap_or(1);
        if start == 0 {
            return Err(ToolError::invalid_args("start_line is counted from one"));
        }
        if args.end_line.is_some_and(|end| end < start) {
            return Err(ToolError::invalid_args("end_line is before start_line"));
        }

        let (lines, stopped, number) = window(&path, &args.path, start, args.end_line)?;
        if lines.is_empty() {
            if number == 0 && start == 1 {
                return Ok(Output {
                    path: args.path,
                    start_line: 1,
                    end_line: 1,
                    lines: 0,
                    content: String::new(),
                    truncated: false,
                });
            }
            return Err(ToolError::not_found(format!(
                "{} has no line {start}",
                args.path
            )));
        }

        Ok(fit(&lines, Self::LIMITS.event_bytes, |kept, over| Output {
            path: args.path.clone(),
            start_line: start,
            end_line: if kept.is_empty() {
                start
            } else {
                start + kept.len() - 1
            },
            lines: kept.len(),
            content: kept.concat(),
            truncated: stopped || over,
        }))
    }
}

/// Reads the requested lines and reports how many lines the file held before
/// the window, so a start past the end can be told from an empty file.
fn window(
    path: &Path,
    named: &str,
    start: usize,
    end: Option<usize>,
) -> Result<(Vec<String>, bool, usize), ToolError> {
    let file = File::open(path).map_err(|error| ToolError::io(path, &error))?;
    // Every pass consumes at least one byte, so the file's own length bounds the
    // walk: a reader that stops advancing ends it rather than spinning on it.
    let bound = file.metadata().map_or(0, |metadata| metadata.len());
    let mut reader = BufReader::new(file);
    let mut lines = Vec::new();
    let mut number = 0;
    let mut content_bytes = 0;
    let mut stopped = false;
    let mut raw = Vec::new();
    for _ in 0..=bound {
        if end.is_some_and(|end| number >= end) {
            break;
        }
        raw.clear();
        let length = line(&mut reader, &mut raw, CONTENT_BYTES)
            .map_err(|error| ToolError::io(path, &error))?;
        if length == 0 {
            break;
        }
        number += 1;
        if number < start {
            continue;
        }
        let overlong = length > CONTENT_BYTES;
        let text = text_of(&raw, overlong, named)?;
        if content_bytes + text.len() > CONTENT_BYTES {
            stopped = true;
            break;
        }
        content_bytes += text.len();
        lines.push(text);
        if overlong {
            stopped = true;
            break;
        }
    }
    Ok((lines, stopped, number))
}

/// Reads one line, storing at most `limit` bytes and reporting the whole length.
fn line(
    reader: &mut impl BufRead,
    raw: &mut Vec<u8>,
    limit: usize,
) -> std::io::Result<usize> {
    let mut total = 0;
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(total);
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let take = newline.map_or(available.len(), |index| index + 1);
        let room = limit.saturating_sub(raw.len());
        raw.extend_from_slice(&available[..take.min(room)]);
        total += take;
        reader.consume(take);
        if newline.is_some() {
            return Ok(total);
        }
    }
}

/// Decodes the bytes, allowing the cut a limit makes in a multi-byte character.
fn text_of(
    raw: &[u8],
    overlong: bool,
    path: &str,
) -> Result<String, ToolError> {
    let error = match std::str::from_utf8(raw) {
        Ok(text) => return Ok(text.to_owned()),
        Err(error) => error,
    };
    if overlong
        && error.error_len().is_none()
        && let Ok(text) = std::str::from_utf8(&raw[..error.valid_up_to()])
    {
        return Ok(text.to_owned());
    }
    Err(ToolError::not_text(format!("{path} is not UTF-8 text")))
}
