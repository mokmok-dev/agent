//! The agent's coding toolbelt: `read`, `code_search`, and `patch` over one
//! workspace, in one **switchable** mode, with `shell` alongside them.
//!
//! A [`Coding`] belt is built from a workspace root and a [`Mode`]. The mode is a
//! mutex cell, so the tool list, the system prompt, and the action gate are all
//! derived from it at each call: a client can switch modes mid-session without any
//! of the three drifting from the others. [`Mode`] is the capability set the agent
//! advertises to the model and to bus clients, not a kernel guarantee. The
//! session's sandbox is the boundary that actually confines a child; see
//! `docs/session/agent.md`. A session the daemon bound read-write stays writable at
//! the kernel, so a read-only belt is a contract, not a wall. The loop is
//! synchronous, so a switch lands between commands; a task already in flight
//! finishes under the mode it started with.
//!
//! Every read of a file or a child is bounded **while it is read**: `read` and
//! `code_search` window each line and stop on byte and count budgets, the `patch`
//! arm reads each base under a byte cap, and the `shell` arm bounds its output the
//! way the OOM incident established.
//!
//! `patch` applies a unified diff in process (see the crate-private `patch`
//! module, which owns the path policy and the caps): `diffy` parses and applies the
//! text, every path is resolved and every file applied into a plan before any write
//! happens. The unified diff stays the wire format the model speaks, and a session's
//! workspace needs no `git` and no repository.

use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Read};
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use serde_json::{Value, json};

use crate::contract::{Command, Output, OutputKind};
use crate::loopcore::{Capability, Reporter};
use crate::model::Tool;
use crate::shell::{ACTION as SHELL_ACTION, Shell};

/// The `shell` action.
pub const ACTION_SHELL: &str = SHELL_ACTION;
/// The `read` action.
pub const ACTION_READ: &str = "read";
/// The `code_search` action.
pub const ACTION_CODE_SEARCH: &str = "code_search";
/// The `patch` action.
pub const ACTION_PATCH: &str = "patch";
/// The session command that reports or switches the belt's mode.
///
/// It is a session command, not a tool: it never appears in [`Coding::tools`], and
/// a model that calls it is told there is no tool by that name.
pub const ACTION_MODE: &str = "mode";

/// How long a child the toolbelt spawns may run before it is killed.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);

/// The most lines a `read` may return.
const MAX_READ_LINES: usize = 2_000;
/// The most content bytes a `read` retains, so the whole detail stays a small JSON.
const MAX_READ_BYTES: usize = 8 * 1024;
/// The most bytes one line of a `read` may contribute.
const MAX_READ_LINE_BYTES: usize = 4 * 1024;
/// The default number of lines a `read` returns.
const DEFAULT_READ_LINES: usize = 200;
/// How many bytes of the `read` detail are reserved for its envelope.
const READ_ENVELOPE_BYTES: usize = 512;

/// The most bytes a `code_search` pattern may have.
const MAX_PATTERN_BYTES: usize = 256;
/// The largest file `code_search` scans; a larger one is skipped, not half-searched.
const MAX_SEARCH_FILE_BYTES: usize = 1024 * 1024;
/// The most directory entries `code_search` visits.
const MAX_SEARCH_FILES: usize = 10_000;
/// The deepest `code_search` recurses.
const MAX_SEARCH_DEPTH: usize = 32;
/// The most matches `code_search` reports.
const MAX_SEARCH_MATCHES: usize = 200;
/// The most match text `code_search` accumulates.
const MAX_SEARCH_TEXT_BYTES: usize = 8 * 1024;
/// The most bytes one reported search line carries.
const MAX_SEARCH_LINE_BYTES: usize = 200;

/// The capability contract a session runs under.
///
/// Every read of the mode derives the offered set from the one cell, so the
/// model's tool list is exactly the actions the belt can dispatch at that moment:
/// a schema can never name an action it would refuse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// The belt can inspect the workspace but cannot change it.
    ReadOnly,
    /// The belt can inspect and change the workspace.
    ReadWrite,
}

impl Mode {
    /// The mode's name, as `--mode` and the model are told it.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::ReadOnly => "readonly",
            Self::ReadWrite => "readwrite",
        }
    }

    /// Whether the belt can change the workspace.
    #[must_use]
    pub const fn is_read_only(self) -> bool {
        matches!(self, Self::ReadOnly)
    }

    /// Whether this mode may run an action that requires `required`.
    const fn covers(
        self,
        required: Self,
    ) -> bool {
        match self {
            Self::ReadWrite => true,
            Self::ReadOnly => matches!(required, Self::ReadOnly),
        }
    }
}

impl std::str::FromStr for Mode {
    type Err = UnknownMode;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "readonly" => Ok(Self::ReadOnly),
            "readwrite" => Ok(Self::ReadWrite),
            other => Err(UnknownMode(other.to_owned())),
        }
    }
}

/// A `--mode` value that is neither `readonly` nor `readwrite`.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error("`{0}` is not a mode: expected `readonly` or `readwrite`")]
pub struct UnknownMode(pub String);

/// Why a belt could not be built.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The workspace root could not be canonicalized as a directory.
    #[error("the workspace root could not be resolved: {0}")]
    Root(#[source] std::io::Error),
}

/// The coding capability: `read`, `code_search`, `patch`, and `shell` over one
/// workspace, gated by one switchable [`Mode`].
#[derive(Debug)]
pub struct Coding {
    /// The canonical workspace root every path is resolved against.
    root: PathBuf,
    /// The session's mode. Switchable while the session runs, so every reader goes
    /// through this one cell: the tool list, the prompt, and the gate cannot drift.
    mode: Mutex<Mode>,
    /// How long a spawned child may run.
    timeout: Duration,
}

impl Coding {
    /// A belt over `root` in `mode`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Root`] when `root` cannot be canonicalized as a directory.
    pub fn new(
        root: impl Into<PathBuf>,
        mode: Mode,
    ) -> Result<Self, Error> {
        let root = std::fs::canonicalize(root.into()).map_err(Error::Root)?;
        if !root.is_dir() {
            return Err(Error::Root(std::io::Error::new(
                std::io::ErrorKind::NotADirectory,
                "the workspace root is not a directory",
            )));
        }
        Ok(Self {
            root,
            mode: Mutex::new(mode),
            timeout: DEFAULT_TIMEOUT,
        })
    }

    /// A belt with an explicit child timeout, for a caller that wants one.
    #[must_use]
    pub const fn with_timeout(
        mut self,
        timeout: Duration,
    ) -> Self {
        self.timeout = timeout;
        self
    }

    /// The mode the belt runs under, recovering a lock a panic poisoned.
    ///
    /// [`Mode`] carries no invariant a panic can break, so the value behind a
    /// poisoned lock is still exactly what was last stored.
    #[must_use]
    pub fn mode(&self) -> Mode {
        *self.mode.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Switch the belt to `mode`, returning the mode it had.
    #[must_use]
    pub fn set_mode(
        &self,
        mode: Mode,
    ) -> Mode {
        std::mem::replace(
            &mut *self.mode.lock().unwrap_or_else(PoisonError::into_inner),
            mode,
        )
    }

    /// The tools this session offers the model, in [`Spec::ALL`] order.
    #[must_use]
    pub fn tools(&self) -> Vec<Tool> {
        let mode = self.mode();
        offered(mode)
            .map(|spec| Tool {
                name: spec.action().to_owned(),
                description: spec.description().to_owned(),
                parameters: spec.parameters(),
            })
            .collect()
    }

    /// What the agent tells the model about itself, for the currently offered set.
    #[must_use]
    pub fn system_prompt(&self) -> String {
        let mode = self.mode();
        let offered: Vec<Spec> = offered(mode).collect();
        let names = offered
            .iter()
            .map(|spec| spec.action())
            .collect::<Vec<_>>()
            .join(", ");
        let clause = if mode.is_read_only() {
            "This session is read-only, so no tool can change the workspace.".to_owned()
        } else {
            // The clause names the writing tools this belt actually offers, so it
            // stays true for every mode.
            let writers = offered
                .iter()
                .filter(|spec| spec.required() == Mode::ReadWrite)
                .map(|spec| format!("`{}`", spec.action()))
                .collect::<Vec<_>>()
                .join(" and ");
            format!("This session may change the workspace, through {writers}.")
        };
        format!(
            "You are the agent for one session, working in that session's workspace. \
You have these tools: {names}. {clause} Use them to inspect the workspace; when the \
task is complete, reply with your answer and no tool call."
        )
    }

    /// Whether the belt currently offers `action`.
    pub(crate) fn offers(
        &self,
        action: &str,
    ) -> bool {
        let mode = self.mode();
        Spec::from_action(action)
            .is_some_and(|spec| offered(mode).any(|candidate| candidate == spec))
    }
}

/// Answer the `mode` action: `detail` is absent, `null`, or `{}` to report the
/// session's mode, or `{"mode": "readonly"|"readwrite"}` to switch it.
///
/// The response names the resulting mode and whether this call changed it, so a
/// client that retries an idempotent switch can tell.
#[must_use]
pub fn act_mode(
    coding: &Coding,
    detail: &Value,
) -> Output {
    match mode_request(detail) {
        ModeRequest::Query => mode_output(coding.mode(), false),
        ModeRequest::Switch(name) => match name.parse::<Mode>() {
            Ok(mode) => {
                let changed = coding.set_mode(mode) != mode;
                mode_output(mode, changed)
            },
            Err(error) => mode_error(&error.to_string()),
        },
        ModeRequest::Malformed => mode_error("malformed mode"),
    }
}

/// What a `mode` action's `detail` asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModeRequest<'a> {
    /// Report the mode without changing it.
    Query,
    /// Switch to the named mode.
    Switch(&'a str),
    /// The detail is not one of the shapes the action accepts.
    Malformed,
}

/// Read the `mode` action's `detail`: absent, `null`, or `{}` is a query, and an
/// object with only a string `mode` is a switch.
fn mode_request(detail: &Value) -> ModeRequest<'_> {
    if detail.is_null() {
        return ModeRequest::Query;
    }
    let Some(fields) = detail.as_object() else {
        return ModeRequest::Malformed;
    };
    if fields.is_empty() {
        return ModeRequest::Query;
    }
    if fields.len() == 1
        && let Some(Value::String(name)) = fields.get("mode")
    {
        return ModeRequest::Switch(name);
    }
    ModeRequest::Malformed
}

/// The `Done` output for a reported or switched mode.
fn mode_output(
    mode: Mode,
    changed: bool,
) -> Output {
    Output {
        kind: OutputKind::Done,
        action: ACTION_MODE.to_owned(),
        detail: json!({"mode": mode.name(), "changed": changed}),
    }
}

/// The `Error` output for a `mode` action that could not be answered.
fn mode_error(reason: &str) -> Output {
    Output {
        kind: OutputKind::Error,
        action: ACTION_MODE.to_owned(),
        detail: json!({"reason": reason}),
    }
}

impl Capability for Coding {
    fn act(
        &self,
        command: &Command,
        reporter: &dyn Reporter,
    ) -> Output {
        let mode = self.mode();
        let Some(spec) = Spec::from_action(&command.action) else {
            return refusal(
                &command.action,
                "the agent has no capability for this action",
            );
        };
        if !offered(mode).any(|candidate| candidate == spec) {
            return refusal(
                &command.action,
                &format!(
                    "`{}` is not available in {} mode",
                    spec.action(),
                    mode.name()
                ),
            );
        }
        spec.run(self, &command.detail, reporter)
    }
}

/// The specs `mode` offers: the one source of truth for the set.
fn offered(mode: Mode) -> impl Iterator<Item = Spec> {
    Spec::ALL
        .into_iter()
        .filter(move |spec| mode.covers(spec.required()))
}

/// The exhaustive routing table: one variant per action, its mode, its schema,
/// and its arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Spec {
    /// The `read` action.
    Read,
    /// The `code_search` action.
    CodeSearch,
    /// The `patch` action.
    Patch,
    /// The `shell` action.
    Shell,
}

impl Spec {
    /// Every spec, in the order the tools are offered.
    const ALL: [Self; 4] = [Self::Read, Self::CodeSearch, Self::Patch, Self::Shell];

    /// The spec an action names, or `None` for an action the belt has no arm for.
    fn from_action(action: &str) -> Option<Self> {
        match action {
            ACTION_READ => Some(Self::Read),
            ACTION_CODE_SEARCH => Some(Self::CodeSearch),
            ACTION_PATCH => Some(Self::Patch),
            ACTION_SHELL => Some(Self::Shell),
            _ => None,
        }
    }

    /// The action name this spec answers.
    const fn action(self) -> &'static str {
        match self {
            Self::Read => ACTION_READ,
            Self::CodeSearch => ACTION_CODE_SEARCH,
            Self::Patch => ACTION_PATCH,
            Self::Shell => ACTION_SHELL,
        }
    }

    /// The mode this spec requires.
    const fn required(self) -> Mode {
        match self {
            Self::Read | Self::CodeSearch => Mode::ReadOnly,
            Self::Patch | Self::Shell => Mode::ReadWrite,
        }
    }

    /// The description the model decides from.
    const fn description(self) -> &'static str {
        match self {
            Self::Read => {
                "Read a window of lines from a file in the workspace, with its line numbers."
            },
            Self::CodeSearch => {
                "Find a literal substring in the workspace's files and report each matching line."
            },
            Self::Patch => {
                "Apply a unified diff to the workspace, checking the diff's syntax first."
            },
            Self::Shell => {
                "Run a program with arguments in the session workspace, and return its exit \
code, its standard output, and its standard error."
            },
        }
    }

    /// The JSON schema of this spec's detail, whose keys are the detail keys.
    fn parameters(self) -> Value {
        match self {
            Self::Read => json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "The workspace-relative path to read.",
                    },
                    "start_line": {
                        "type": "integer",
                        "description": "The first line to return, 1-based. Defaults to 1.",
                    },
                    "line_count": {
                        "type": "integer",
                        "description": "How many lines to return, at most 2000. Defaults to 200.",
                    },
                },
                "required": ["path"],
                "additionalProperties": false,
            }),
            Self::CodeSearch => json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "A literal substring to find; this is not a regular expression.",
                    },
                    "path": {
                        "type": "string",
                        "description": "A workspace-relative file or directory to search. Defaults to the root.",
                    },
                },
                "required": ["pattern"],
                "additionalProperties": false,
            }),
            Self::Patch => json!({
                "type": "object",
                "properties": {
                    "diff": {
                        "type": "string",
                        "description": "A unified diff to apply to the workspace.",
                    },
                },
                "required": ["diff"],
                "additionalProperties": false,
            }),
            Self::Shell => json!({
                "type": "object",
                "properties": {
                    "argv": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "The program and its arguments, the program first.",
                    },
                },
                "required": ["argv"],
                "additionalProperties": false,
            }),
        }
    }

    /// Run this spec's arm.
    fn run(
        self,
        coding: &Coding,
        detail: &Value,
        reporter: &dyn Reporter,
    ) -> Output {
        match self {
            Self::Read => read(coding, detail),
            Self::CodeSearch => code_search(coding, detail),
            Self::Patch => patch(coding, detail, reporter),
            Self::Shell => run_shell(coding, detail, reporter),
        }
    }
}

/// The output for a refused action.
fn refusal(
    action: &str,
    reason: &str,
) -> Output {
    Output {
        kind: OutputKind::Error,
        action: action.to_owned(),
        detail: json!({"reason": reason}),
    }
}

/// Read a window of lines from one file, bounded while it is read.
fn read(
    coding: &Coding,
    detail: &Value,
) -> Output {
    let Some(raw_path) = detail.get("path").and_then(Value::as_str) else {
        return refusal(ACTION_READ, "malformed read");
    };
    let start_line = detail
        .get("start_line")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .max(1);
    let requested = detail
        .get("line_count")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_READ_LINES as u64);
    let line_count =
        usize::try_from(requested.min(MAX_READ_LINES as u64)).unwrap_or(MAX_READ_LINES);

    let path = match resolve(coding, raw_path) {
        Ok(path) => path,
        Err(error) => return refusal(ACTION_READ, error.reason()),
    };
    let outcome = match read_file(&path.0, raw_path, start_line, line_count) {
        Ok(outcome) => outcome,
        Err(reason) => return refusal(ACTION_READ, &reason),
    };
    Output {
        kind: OutputKind::Done,
        action: ACTION_READ.to_owned(),
        detail: json!({
            "path": raw_path,
            "start_line": start_line,
            "lines": outcome.lines,
            "content": outcome.content,
            "next_line": outcome.next_line,
            "truncated": outcome.truncated,
        }),
    }
}

/// What one bounded file read retained.
struct ReadOutcome {
    /// The retained text.
    content: String,
    /// How many lines were returned.
    lines: usize,
    /// The line to start the next read from, or `None` at end of input.
    next_line: Option<u64>,
    /// Whether a bound stopped the read before the input ended.
    truncated: bool,
}

/// Read up to `line_count` lines from `start_line`, bounded by
/// [`MAX_READ_LINE_BYTES`] per line and [`MAX_READ_BYTES`] overall.
///
/// # Errors
///
/// Returns the model-facing reason when the path is not a regular file or holds a
/// NUL byte.
fn read_file(
    path: &Path,
    raw: &str,
    start_line: u64,
    line_count: usize,
) -> Result<ReadOutcome, String> {
    let metadata = std::fs::metadata(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            format!("no file at `{raw}`")
        } else {
            format!("`{raw}` is not a regular file")
        }
    })?;
    if !metadata.is_file() {
        return Err(format!("`{raw}` is not a regular file"));
    }
    let file = std::fs::File::open(path).map_err(|_| format!("no file at `{raw}`"))?;
    let mut reader = BufReader::new(file);

    // The budget is on the escaped size, so the serialized detail stays within
    // MAX_READ_BYTES even when the content is full of quotes and newlines.
    let budget = MAX_READ_BYTES - READ_ENVELOPE_BYTES;
    let mut content = String::new();
    let mut escaped = 0_usize;
    let mut lines = 0_usize;
    let mut truncated = false;
    let mut next_line = None;
    // The lines before the window are consumed but not retained. Counting up to
    // `start_line` and comparing would leave state that nothing reads once the
    // window has started, so the countdown is the whole of it.
    let mut to_skip = start_line - 1;

    loop {
        if lines >= line_count {
            if reader.fill_buf().is_ok_and(|buffered| !buffered.is_empty()) {
                truncated = true;
                next_line = Some(start_line + lines as u64);
            }
            break;
        }
        let Some((buffer, too_long)) =
            read_line_window(&mut reader).map_err(|_| format!("no file at `{raw}`"))?
        else {
            break;
        };
        if to_skip > 0 {
            if too_long {
                discard_line(&mut reader).map_err(|_| format!("no file at `{raw}`"))?;
            }
            to_skip -= 1;
            continue;
        }

        let kept = if too_long {
            &buffer[..MAX_READ_LINE_BYTES.min(buffer.len())]
        } else {
            &buffer[..]
        };
        if kept.contains(&0) {
            return Err(format!("`{raw}` is not text"));
        }
        let chunk = String::from_utf8_lossy(kept);
        let cost = escaped_len(&chunk);
        if escaped + cost > budget {
            truncated = true;
            next_line = Some(start_line + lines as u64);
            break;
        }
        content.push_str(&chunk);
        escaped += cost;
        lines += 1;
        if too_long {
            truncated = true;
            discard_line(&mut reader).map_err(|_| format!("no file at `{raw}`"))?;
        }
    }

    Ok(ReadOutcome {
        content,
        lines,
        next_line,
        truncated,
    })
}

/// Search the workspace for a literal substring.
fn code_search(
    coding: &Coding,
    detail: &Value,
) -> Output {
    let Some(pattern) = detail.get("pattern").and_then(Value::as_str) else {
        return refusal(ACTION_CODE_SEARCH, "malformed code_search");
    };
    if pattern.is_empty() {
        return refusal(ACTION_CODE_SEARCH, "the pattern is empty");
    }
    if pattern.len() > MAX_PATTERN_BYTES {
        return refusal(ACTION_CODE_SEARCH, "the pattern is too long");
    }
    let raw_path = detail.get("path").and_then(Value::as_str).unwrap_or(".");
    let path = match resolve(coding, raw_path) {
        Ok(path) => path,
        Err(error) => return refusal(ACTION_CODE_SEARCH, error.reason()),
    };

    let mut search = Search {
        pattern,
        root: &coding.root,
        matches: Vec::new(),
        files: 0,
        skipped: 0,
        text_bytes: 0,
        visited: 0,
        truncated: false,
    };
    match std::fs::metadata(&path.0) {
        Ok(metadata) if metadata.is_file() => search_file(&mut search, &path.0),
        Ok(metadata) if metadata.is_dir() => search_dir(&mut search, &path.0, 0),
        _ => {
            return refusal(
                ACTION_CODE_SEARCH,
                &format!("no file or directory at `{raw_path}`"),
            );
        },
    }

    Output {
        kind: OutputKind::Done,
        action: ACTION_CODE_SEARCH.to_owned(),
        detail: json!({
            "pattern": pattern,
            "matches": search.matches,
            "files": search.files,
            "skipped": search.skipped,
            "truncated": search.truncated,
        }),
    }
}

/// One bounded search's accumulation.
struct Search<'a> {
    /// The literal substring to find.
    pattern: &'a str,
    /// The workspace root, to keep reported paths relative.
    root: &'a Path,
    /// The matches found so far.
    matches: Vec<Value>,
    /// How many regular files were searched.
    files: usize,
    /// How many files were skipped, because they were too large or unreadable.
    skipped: usize,
    /// How many bytes of match text were reported.
    text_bytes: usize,
    /// How many directory entries were visited.
    visited: usize,
    /// Whether the search may have missed a match: a bound stopped it, a line was
    /// too long to hold whole, or an entry could not be read.
    truncated: bool,
}

/// Whether a hard search bound is reached, which stops the walk.
///
/// A walk that stops at a bound reports `truncated`, so a partial search is never
/// read as the whole workspace. A line too long to hold and an unreadable entry
/// mark the result incomplete without stopping the walk, because the rest of the
/// tree is still worth searching.
const fn at_bound(ctx: &Search) -> bool {
    ctx.visited >= MAX_SEARCH_FILES
        || ctx.matches.len() >= MAX_SEARCH_MATCHES
        || ctx.text_bytes >= MAX_SEARCH_TEXT_BYTES
}

/// Walk `dir` depth-first, in file-name order, skipping `.git` and symlinks.
fn search_dir(
    ctx: &mut Search,
    dir: &Path,
    depth: usize,
) {
    if at_bound(ctx) {
        ctx.truncated = true;
        return;
    }
    if depth > MAX_SEARCH_DEPTH {
        ctx.truncated = true;
        return;
    }
    let Ok(listing) = std::fs::read_dir(dir) else {
        // A directory that cannot be read leaves the search incomplete, so the
        // result must not be read as the whole workspace.
        ctx.truncated = true;
        return;
    };
    let mut entries = Vec::new();
    for entry in listing {
        match entry {
            Ok(entry) => entries.push(entry),
            // An entry that cannot be read leaves the search incomplete, for the
            // same reason a directory that cannot be read does.
            Err(_) => ctx.truncated = true,
        }
    }
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        if at_bound(ctx) {
            ctx.truncated = true;
            return;
        }
        ctx.visited += 1;
        if entry.file_name() == OsStr::new(".git") {
            continue;
        }
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        // `DirEntry::file_type` does not follow a symlink, so a link is skipped
        // rather than walked out of the workspace.
        if file_type.is_symlink() {
            continue;
        }
        let path = entry.path();
        if file_type.is_dir() {
            search_dir(ctx, &path, depth + 1);
        } else if file_type.is_file() {
            search_file(ctx, &path);
        }
    }
}

/// Scan one file line by line, bounded while it is read.
fn search_file(
    ctx: &mut Search,
    path: &Path,
) {
    if at_bound(ctx) {
        ctx.truncated = true;
        return;
    }
    // A file too large to hold whole, one that is not a regular file, and one that
    // cannot be opened are left out together and counted together: a half-searched
    // file would report a match count that is not the truth, and one increment is one
    // behaviour to test.
    let worth_reading = std::fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.len() <= MAX_SEARCH_FILE_BYTES as u64);
    let file = if worth_reading {
        std::fs::File::open(path).ok()
    } else {
        None
    };
    let Some(file) = file else {
        ctx.skipped += 1;
        return;
    };
    ctx.files += 1;
    let relative = path
        .strip_prefix(ctx.root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned();
    let mut reader = BufReader::new(file);
    let mut line = 1_u64;

    loop {
        if at_bound(ctx) {
            ctx.truncated = true;
            return;
        }
        let (buffer, too_long) = match read_line_window(&mut reader) {
            Ok(Some(window)) => window,
            Ok(None) => return,
            // A file that cannot be read to its end leaves the search incomplete.
            Err(_) => {
                ctx.truncated = true;
                return;
            },
        };
        if too_long {
            // A match could hide past the window, but the lines after it are still
            // worth searching, so this marks the result incomplete and reads on.
            ctx.truncated = true;
        }
        let kept = if too_long {
            &buffer[..MAX_READ_LINE_BYTES.min(buffer.len())]
        } else {
            &buffer[..]
        };
        let text = String::from_utf8_lossy(kept);
        let text = text.trim_end_matches(['\n', '\r']);
        if text.contains(ctx.pattern) {
            // The whole windowed line is matched; only the reported text is cut,
            // so a match past the report cap is still found.
            let reported = cut_chars(text, MAX_SEARCH_LINE_BYTES);
            if ctx.matches.len() >= MAX_SEARCH_MATCHES
                || ctx.text_bytes + reported.len() > MAX_SEARCH_TEXT_BYTES
            {
                ctx.truncated = true;
                return;
            }
            ctx.text_bytes += reported.len();
            ctx.matches.push(json!({
                "path": relative,
                "line": line,
                "text": reported,
            }));
        }
        if too_long {
            let _ = discard_line(&mut reader);
        }
        line += 1;
    }
}

/// Apply a unified diff to the workspace in process.
///
/// The whole patch is planned by [`crate::patch::build`] before anything is
/// written, so a refusal never follows a write; the files are reported before the
/// plan is committed, so a watcher sees what is about to change.
fn patch(
    coding: &Coding,
    detail: &Value,
    reporter: &dyn Reporter,
) -> Output {
    let Some(source) = detail.get("diff").and_then(Value::as_str) else {
        return refusal(ACTION_PATCH, "malformed patch");
    };
    let plan = match crate::patch::build(coding, source) {
        Ok(plan) => plan,
        Err(refused) => return patch_refusal(&refused),
    };
    let paths = plan.paths();
    reporter.report(json!({"files": &paths}));
    match plan.commit() {
        Ok(files_changed) => Output {
            kind: OutputKind::Done,
            action: ACTION_PATCH.to_owned(),
            detail: json!({"files": paths, "files_changed": files_changed}),
        },
        Err(refused) => patch_refusal(&refused),
    }
}

/// The `Error` output for a patch the plan or a write refused.
fn patch_refusal(refused: &crate::patch::Refused) -> Output {
    let mut detail = json!({"reason": refused.to_string()});
    if let Some(error) = refused.error() {
        detail["error"] = json!(error);
    }
    Output {
        kind: OutputKind::Error,
        action: ACTION_PATCH.to_owned(),
        detail,
    }
}

/// Run the `shell` arm through a [`Shell`] over the same workspace and timeout.
fn run_shell(
    coding: &Coding,
    detail: &Value,
    reporter: &dyn Reporter,
) -> Output {
    let shell = Shell::in_workspace(&coding.root).with_timeout(coding.timeout);
    let command = Command {
        action: ACTION_SHELL.to_owned(),
        detail: detail.clone(),
    };
    shell.act(&command, reporter)
}

/// A path proven to resolve under the workspace root.
#[derive(Debug, Clone)]
struct WorkspacePath(PathBuf);

/// Why a path could not be resolved under the workspace.
#[derive(Debug, thiserror::Error)]
pub(crate) enum PathError {
    /// The path has no components.
    #[error("the path is empty")]
    Empty,
    /// The path is absolute rather than workspace-relative.
    #[error("the path is absolute")]
    Absolute,
    /// The path contains a `..` component.
    #[error("the path contains a parent component")]
    Parent,
    /// The path contains a NUL byte.
    #[error("the path contains a null byte")]
    Nul,
    /// The path resolves outside the workspace root.
    #[error("the path leaves the workspace")]
    Escape,
    /// The path names `.git`, which a write may not touch.
    #[error("`.git` is protected")]
    Protected,
}

impl PathError {
    /// The reason, suitable for telling the model.
    pub(crate) const fn reason(&self) -> &'static str {
        match self {
            Self::Empty => "the path is empty",
            Self::Absolute => "the path is absolute",
            Self::Parent => "the path contains a parent component",
            Self::Nul => "the path contains a null byte",
            Self::Escape => "the path leaves the workspace",
            Self::Protected => "`.git` is protected",
        }
    }
}

/// Resolve a read path under the workspace root.
///
/// The returned path is the canonicalized deepest existing ancestor with the
/// non-existing tail re-appended, so an open cannot re-follow a symlink that was
/// live when the path was checked.
fn resolve(
    coding: &Coding,
    raw: &str,
) -> Result<WorkspacePath, PathError> {
    if raw.is_empty() {
        return Err(PathError::Empty);
    }
    if raw.contains('\0') {
        return Err(PathError::Nul);
    }
    let mut relative = PathBuf::new();
    for component in Path::new(raw).components() {
        match component {
            Component::RootDir | Component::Prefix(_) => return Err(PathError::Absolute),
            Component::ParentDir => return Err(PathError::Parent),
            Component::CurDir => {},
            Component::Normal(part) => relative.push(part),
        }
    }
    // A path of only `.` components is the workspace root itself.
    resolve_under(&coding.root, &relative)
}

/// Resolve a write path under the workspace root, additionally refusing `.git`.
pub(crate) fn resolve_for_write(
    coding: &Coding,
    raw: &str,
) -> Result<PathBuf, PathError> {
    let resolved = resolve(coding, raw)?;
    if Path::new(raw)
        .components()
        .any(|component| matches!(component, Component::Normal(part) if part == OsStr::new(".git")))
    {
        return Err(PathError::Protected);
    }
    Ok(resolved.0)
}

/// Canonicalize the deepest existing ancestor of `root/relative` and re-append
/// the non-existing tail.
fn resolve_under(
    root: &Path,
    relative: &Path,
) -> Result<WorkspacePath, PathError> {
    let joined = root.join(relative);
    let mut existing: &Path = &joined;
    let mut tail: Vec<&OsStr> = Vec::new();
    loop {
        match std::fs::canonicalize(existing) {
            Ok(canonical) => {
                let mut resolved = canonical;
                for part in tail.iter().rev() {
                    resolved.push(part);
                }
                if !resolved.starts_with(root) {
                    return Err(PathError::Escape);
                }
                return Ok(WorkspacePath(resolved));
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let Some(parent) = existing.parent() else {
                    return Err(PathError::Escape);
                };
                let Some(name) = existing.file_name() else {
                    return Err(PathError::Escape);
                };
                tail.push(name);
                existing = parent;
            },
            Err(_) => return Err(PathError::Escape),
        }
    }
}

/// Read one line from `reader`, bounded to [`MAX_READ_LINE_BYTES`] plus one byte.
///
/// `Ok(None)` is end of input. The second value is true when the window filled
/// without a newline, so the line is longer than the cap and its remainder must
/// still be discarded.
fn read_line_window<R: BufRead>(reader: &mut R) -> std::io::Result<Option<(Vec<u8>, bool)>> {
    let mut buffer = Vec::new();
    let mut window = (&mut *reader).take(MAX_READ_LINE_BYTES as u64 + 1);
    window.read_until(b'\n', &mut buffer)?;
    // Nothing appended is the end of the input, and asking the buffer says so without
    // a count a mutant could invert into reading forever.
    if buffer.is_empty() {
        return Ok(None);
    }
    // The window is one byte wider than the cap, so a line of exactly the cap plus
    // its newline fills it while still ending in a newline; only a window that
    // filled without one means the line is too long.
    let too_long = buffer.len() > MAX_READ_LINE_BYTES && !buffer.ends_with(b"\n");
    Ok(Some((buffer, too_long)))
}

/// Discard the rest of a line up to and including its newline.
///
/// `consume` stops exactly after the newline, so the line that follows a line too
/// long to keep is neither dropped nor read from the middle.
fn discard_line<R: BufRead>(reader: &mut R) -> std::io::Result<()> {
    loop {
        let buffered = reader.fill_buf()?;
        if buffered.is_empty() {
            return Ok(());
        }
        if let Some(index) = buffered.iter().position(|byte| *byte == b'\n') {
            reader.consume(index + 1);
            return Ok(());
        }
        let drained = buffered.len();
        reader.consume(drained);
    }
}

/// `text` cut to at most `cap` bytes on a character boundary.
fn cut_chars(
    text: &str,
    cap: usize,
) -> String {
    if text.len() <= cap {
        return text.to_owned();
    }
    let end = text.floor_char_boundary(cap);
    text[..end].to_owned()
}

/// How many bytes `text` occupies inside a JSON string, without its quotes.
fn escaped_len(text: &str) -> usize {
    text.chars().map(escape_cost).sum()
}

/// How many bytes one character occupies inside a JSON string, without quotes.
fn escape_cost(ch: char) -> usize {
    match ch {
        '"' | '\\' | '\n' | '\r' | '\t' => 2,
        ch if u32::from(ch) < 0x20 => 6,
        ch => ch.len_utf8(),
    }
}

#[cfg(test)]
mod tests {
    // Tests for the coding belt: the mode gate, the path guard, and each arm over
    // a real workspace. `patch` runs the host's real `git`; `read` and
    // `code_search` run over real files.

    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    /// A temp workspace, removed on drop.
    struct Workspace(PathBuf);

    impl Workspace {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "agent-coding-{tag}-{}-{unique}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("creates the workspace");
            Self(path)
        }

        /// Write `contents` to `rel`, creating its parents.
        fn write(
            &self,
            rel: &str,
            contents: &str,
        ) {
            let path = self.0.join(rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("creates the parents");
            }
            std::fs::write(&path, contents).expect("writes the file");
        }

        /// Write raw `bytes` to `rel`, creating its parents.
        fn write_bytes(
            &self,
            rel: &str,
            bytes: &[u8],
        ) {
            let path = self.0.join(rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("creates the parents");
            }
            std::fs::write(&path, bytes).expect("writes the file");
        }
    }

    impl Drop for Workspace {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A reporter that records what it was told.
    #[derive(Debug, Default)]
    struct Reports(Mutex<Vec<Value>>);

    impl Reports {
        /// The details reported, in order.
        fn details(&self) -> Vec<Value> {
            self.0.lock().expect("unpoisoned").clone()
        }
    }

    impl Reporter for Reports {
        fn report(
            &self,
            detail: Value,
        ) {
            self.0.lock().expect("unpoisoned").push(detail);
        }
    }

    /// A belt over `workspace` in `mode`.
    fn belt(
        workspace: &Workspace,
        mode: Mode,
    ) -> Coding {
        Coding::new(&workspace.0, mode).expect("the workspace opens")
    }

    /// Run `action` on `coding` and return the output.
    fn act(
        coding: &Coding,
        action: &str,
        detail: Value,
    ) -> Output {
        coding.act(
            &Command {
                action: action.to_owned(),
                detail,
            },
            &Reports::default(),
        )
    }

    /// The names of the tools a belt offers, in order.
    fn tool_names(coding: &Coding) -> Vec<String> {
        coding.tools().into_iter().map(|tool| tool.name).collect()
    }

    #[test]
    fn a_readonly_belt_offers_read_and_code_search() {
        let workspace = Workspace::new("ro-tools");
        let coding = belt(&workspace, Mode::ReadOnly);
        assert_eq!(tool_names(&coding), ["read", "code_search"]);
    }

    #[test]
    fn every_tool_carries_the_description_the_model_decides_from() {
        let workspace = Workspace::new("descriptions");
        let coding = belt(&workspace, Mode::ReadWrite);
        let descriptions: Vec<String> = coding
            .tools()
            .into_iter()
            .map(|tool| tool.description)
            .collect();
        assert_eq!(
            descriptions,
            [
                "Read a window of lines from a file in the workspace, with its line numbers.",
                "Find a literal substring in the workspace's files and report each matching line.",
                "Apply a unified diff to the workspace, checking the diff's syntax first.",
                "Run a program with arguments in the session workspace, and return its exit code, its standard output, and its standard error.",
            ]
        );
    }

    #[test]
    fn a_readwrite_belt_offers_all_four() {
        let workspace = Workspace::new("rw-tools");
        let coding = belt(&workspace, Mode::ReadWrite);
        assert_eq!(
            tool_names(&coding),
            ["read", "code_search", "patch", "shell"]
        );
    }

    #[test]
    fn the_tools_are_exactly_the_offered_actions() {
        let workspace = Workspace::new("tools-offered");
        let coding = belt(&workspace, Mode::ReadWrite);
        for tool in coding.tools() {
            assert!(coding.offers(&tool.name), "{} is offered", tool.name);
        }
        assert!(!coding.offers("teleport"));
    }

    #[test]
    fn offers_follows_the_mode_for_every_action() {
        let workspace = Workspace::new("offers-mode");
        let readonly = belt(&workspace, Mode::ReadOnly);
        let readwrite = belt(&workspace, Mode::ReadWrite);
        for action in [ACTION_READ, ACTION_CODE_SEARCH] {
            assert!(readonly.offers(action), "readonly offers {action}");
            assert!(readwrite.offers(action), "readwrite offers {action}");
        }
        for action in [ACTION_PATCH, ACTION_SHELL] {
            assert!(!readonly.offers(action), "readonly withholds {action}");
            assert!(readwrite.offers(action), "readwrite offers {action}");
        }
    }

    #[test]
    fn a_readonly_belt_refuses_shell_with_the_mode_reason() {
        let workspace = Workspace::new("ro-shell");
        let coding = belt(&workspace, Mode::ReadOnly);
        let output = act(
            &coding,
            ACTION_SHELL,
            json!({"argv": ["/bin/sh", "-c", "exit 0"]}),
        );
        assert_eq!(output.kind, OutputKind::Error);
        assert_eq!(output.action, "shell");
        assert_eq!(
            output.detail["reason"],
            "`shell` is not available in readonly mode"
        );
    }

    #[test]
    fn a_readonly_belt_refuses_patch_with_the_mode_reason() {
        let workspace = Workspace::new("ro-patch");
        let coding = belt(&workspace, Mode::ReadOnly);
        let output = act(&coding, ACTION_PATCH, json!({"diff": "anything"}));
        assert_eq!(output.kind, OutputKind::Error);
        assert_eq!(
            output.detail["reason"],
            "`patch` is not available in readonly mode"
        );
    }

    #[test]
    fn a_readonly_belt_still_reads() {
        let workspace = Workspace::new("ro-read");
        workspace.write("f.txt", "hello\n");
        let coding = belt(&workspace, Mode::ReadOnly);
        let output = act(&coding, ACTION_READ, json!({"path": "f.txt"}));
        assert_eq!(output.kind, OutputKind::Done);
        assert_eq!(output.detail["content"], "hello\n");
    }

    #[test]
    fn an_unknown_action_is_refused() {
        let workspace = Workspace::new("unknown");
        let coding = belt(&workspace, Mode::ReadWrite);
        let output = act(&coding, "teleport", json!({}));
        assert_eq!(output.kind, OutputKind::Error);
        assert_eq!(
            output.detail["reason"],
            "the agent has no capability for this action"
        );
    }

    #[test]
    fn the_mode_names_parse_and_report_unknown_values() {
        assert_eq!("readonly".parse::<Mode>(), Ok(Mode::ReadOnly));
        assert_eq!("readwrite".parse::<Mode>(), Ok(Mode::ReadWrite));
        let error = "sideways".parse::<Mode>().expect_err("not a mode");
        assert_eq!(
            error.to_string(),
            "`sideways` is not a mode: expected `readonly` or `readwrite`"
        );
        assert_eq!(Mode::ReadOnly.name(), "readonly");
        assert_eq!(Mode::ReadWrite.name(), "readwrite");
        assert!(Mode::ReadOnly.is_read_only());
        assert!(!Mode::ReadWrite.is_read_only());
    }

    #[test]
    fn the_prompt_names_the_offered_tools() {
        let workspace = Workspace::new("prompt");
        let coding = belt(&workspace, Mode::ReadOnly);
        let prompt = coding.system_prompt();
        assert!(prompt.contains("read, code_search"), "prompt was: {prompt}");
        assert!(
            prompt.contains("read-only"),
            "the read-only clause is stated: {prompt}"
        );
    }

    #[test]
    fn a_readwrite_prompt_names_the_writers() {
        let workspace = Workspace::new("prompt-writers");
        let prompt = belt(&workspace, Mode::ReadWrite).system_prompt();
        assert!(
            prompt.contains("through `patch` and `shell`"),
            "both writers are named: {prompt}"
        );
    }

    #[test]
    fn the_mode_query_reports_the_mode_without_changing_it() {
        let workspace = Workspace::new("mode-query");
        let coding = belt(&workspace, Mode::ReadWrite);
        let output = act_mode(&coding, &Value::Null);
        assert_eq!(output.kind, OutputKind::Done);
        assert_eq!(output.action, ACTION_MODE);
        assert_eq!(
            output.detail,
            json!({"mode": "readwrite", "changed": false})
        );
        assert_eq!(coding.mode(), Mode::ReadWrite);
    }

    #[test]
    fn an_absent_and_an_empty_mode_detail_report_the_mode() {
        let workspace = Workspace::new("mode-empty");
        let coding = belt(&workspace, Mode::ReadOnly);
        for detail in [Value::Null, json!({})] {
            let output = act_mode(&coding, &detail);
            assert_eq!(output.kind, OutputKind::Done);
            assert_eq!(output.detail, json!({"mode": "readonly", "changed": false}));
        }
    }

    #[test]
    fn a_mode_switch_changes_the_mode_and_reports_it() {
        let workspace = Workspace::new("mode-switch");
        let coding = belt(&workspace, Mode::ReadWrite);
        let output = act_mode(&coding, &json!({"mode": "readonly"}));
        assert_eq!(output.kind, OutputKind::Done);
        assert_eq!(output.detail, json!({"mode": "readonly", "changed": true}));
        assert_eq!(coding.mode(), Mode::ReadOnly);
    }

    #[test]
    fn switching_to_the_current_mode_is_idempotent() {
        let workspace = Workspace::new("mode-idempotent");
        let coding = belt(&workspace, Mode::ReadOnly);
        let output = act_mode(&coding, &json!({"mode": "readonly"}));
        assert_eq!(output.kind, OutputKind::Done);
        assert_eq!(output.detail, json!({"mode": "readonly", "changed": false}));
        assert_eq!(coding.mode(), Mode::ReadOnly);
    }

    #[test]
    fn a_malformed_mode_detail_is_refused() {
        let workspace = Workspace::new("mode-malformed");
        let coding = belt(&workspace, Mode::ReadWrite);
        for detail in [
            json!(7),
            json!("readonly"),
            json!({"mode": 7}),
            json!({"mode": "readonly", "x": 1}),
            json!({"other": "readonly"}),
        ] {
            let output = act_mode(&coding, &detail);
            assert_eq!(output.kind, OutputKind::Error, "detail: {detail}");
            assert_eq!(output.detail["reason"], "malformed mode");
        }
        assert_eq!(
            coding.mode(),
            Mode::ReadWrite,
            "a malformed detail never switches"
        );
    }

    #[test]
    fn an_unknown_mode_name_is_refused() {
        let workspace = Workspace::new("mode-unknown");
        let coding = belt(&workspace, Mode::ReadWrite);
        let output = act_mode(&coding, &json!({"mode": "sideways"}));
        assert_eq!(output.kind, OutputKind::Error);
        assert_eq!(
            output.detail["reason"],
            "`sideways` is not a mode: expected `readonly` or `readwrite`"
        );
        assert_eq!(coding.mode(), Mode::ReadWrite);
    }

    #[test]
    fn a_switch_to_readonly_gates_patch_and_shell_but_not_read() {
        let workspace = Workspace::new("mode-gate");
        workspace.write("f.txt", "hello\n");
        let coding = belt(&workspace, Mode::ReadWrite);
        assert_eq!(coding.set_mode(Mode::ReadOnly), Mode::ReadWrite);

        let patch = act(&coding, ACTION_PATCH, json!({"diff": "anything"}));
        assert_eq!(patch.kind, OutputKind::Error);
        assert_eq!(
            patch.detail["reason"],
            "`patch` is not available in readonly mode"
        );

        let shell = act(
            &coding,
            ACTION_SHELL,
            json!({"argv": ["/bin/sh", "-c", "echo hi"]}),
        );
        assert_eq!(shell.kind, OutputKind::Error);
        assert_eq!(
            shell.detail["reason"],
            "`shell` is not available in readonly mode"
        );

        let read = act(&coding, ACTION_READ, json!({"path": "f.txt"}));
        assert_eq!(read.kind, OutputKind::Done);
        assert_eq!(read.detail["content"], "hello\n");

        assert_eq!(coding.set_mode(Mode::ReadWrite), Mode::ReadOnly);
        assert!(coding.offers(ACTION_PATCH), "a switch back offers `patch`");
    }

    #[test]
    fn the_tools_and_prompt_follow_a_switch() {
        let workspace = Workspace::new("mode-tools");
        let coding = belt(&workspace, Mode::ReadWrite);
        assert_eq!(
            tool_names(&coding),
            ["read", "code_search", "patch", "shell"]
        );
        assert!(
            coding
                .system_prompt()
                .contains("through `patch` and `shell`"),
            "the writers are named while writable"
        );

        assert_eq!(coding.set_mode(Mode::ReadOnly), Mode::ReadWrite);
        assert_eq!(tool_names(&coding), ["read", "code_search"]);
        let prompt = coding.system_prompt();
        assert!(prompt.contains("read-only"), "prompt was: {prompt}");
        assert!(
            !prompt.contains("patch"),
            "the writing tool is gone from the prompt: {prompt}"
        );
    }

    #[test]
    fn mode_is_not_a_tool() {
        let workspace = Workspace::new("mode-tool");
        let coding = belt(&workspace, Mode::ReadWrite);
        assert!(!coding.offers(ACTION_MODE));
        assert!(
            !tool_names(&coding).contains(&ACTION_MODE.to_owned()),
            "the session command never appears as a tool"
        );
        let output = act(&coding, ACTION_MODE, json!({}));
        assert_eq!(output.kind, OutputKind::Error);
        assert_eq!(
            output.detail["reason"],
            "the agent has no capability for this action"
        );
    }

    #[test]
    fn an_empty_path_is_refused() {
        let workspace = Workspace::new("empty-path");
        let coding = belt(&workspace, Mode::ReadOnly);
        assert_eq!(
            resolve(&coding, "").expect_err("empty").reason(),
            "the path is empty"
        );
    }

    #[test]
    fn an_absolute_path_is_refused() {
        let workspace = Workspace::new("absolute-path");
        let coding = belt(&workspace, Mode::ReadOnly);
        assert_eq!(
            resolve(&coding, "/etc/passwd")
                .expect_err("absolute")
                .reason(),
            "the path is absolute"
        );
    }

    #[test]
    fn a_parent_path_is_refused() {
        let workspace = Workspace::new("parent-path");
        let coding = belt(&workspace, Mode::ReadOnly);
        assert_eq!(
            resolve(&coding, "../x").expect_err("parent").reason(),
            "the path contains a parent component"
        );
    }

    #[test]
    fn a_null_byte_path_is_refused() {
        let workspace = Workspace::new("nul-path");
        let coding = belt(&workspace, Mode::ReadOnly);
        assert_eq!(
            resolve(&coding, "a\0b").expect_err("nul").reason(),
            "the path contains a null byte"
        );
    }

    #[test]
    fn a_symlink_out_of_the_workspace_is_refused() {
        let workspace = Workspace::new("symlink-escape");
        std::os::unix::fs::symlink("/etc", workspace.0.join("escape")).expect("makes the link");
        let coding = belt(&workspace, Mode::ReadOnly);
        assert_eq!(
            resolve(&coding, "escape/passwd")
                .expect_err("escape")
                .reason(),
            "the path leaves the workspace"
        );
    }

    #[test]
    fn a_git_directory_is_protected_from_writes_but_readable() {
        let workspace = Workspace::new("git-protected");
        workspace.write(".git/config", "[core]\n");
        let coding = belt(&workspace, Mode::ReadWrite);
        assert!(
            resolve(&coding, ".git/config").is_ok(),
            "a read may look inside .git"
        );
        assert_eq!(
            resolve_for_write(&coding, ".git/config")
                .expect_err("protected")
                .reason(),
            "`.git` is protected"
        );
        assert!(
            resolve_for_write(&coding, "gitignore.txt").is_ok(),
            "a name merely starting with `.git` is not protected"
        );
    }

    #[test]
    fn a_read_returns_the_requested_lines() {
        let workspace = Workspace::new("read-lines");
        workspace.write("f.txt", "alpha\nbeta\ngamma\n");
        let coding = belt(&workspace, Mode::ReadOnly);
        let output = act(&coding, ACTION_READ, json!({"path": "f.txt"}));
        assert_eq!(output.kind, OutputKind::Done);
        assert_eq!(output.detail["path"], "f.txt");
        assert_eq!(output.detail["start_line"], 1);
        assert_eq!(output.detail["lines"], 3);
        assert_eq!(output.detail["content"], "alpha\nbeta\ngamma\n");
        assert_eq!(output.detail["next_line"], Value::Null);
        assert_eq!(output.detail["truncated"], false);
    }

    #[test]
    fn a_read_window_of_exactly_the_line_cap_is_kept_whole() {
        let workspace = Workspace::new("read-line-exact");
        let line = "x".repeat(MAX_READ_LINE_BYTES);
        workspace.write("f.txt", &format!("{line}\n"));
        let coding = belt(&workspace, Mode::ReadOnly);
        let output = act(
            &coding,
            ACTION_READ,
            json!({"path": "f.txt", "line_count": 1}),
        );
        assert_eq!(
            output.detail["content"].as_str().expect("a string").len(),
            MAX_READ_LINE_BYTES + 1,
            "a line of exactly the cap plus its newline is kept whole"
        );
        assert_eq!(output.detail["truncated"], false);
    }

    #[test]
    fn a_read_window_one_byte_over_the_line_cap_is_cut_and_marked() {
        let workspace = Workspace::new("read-line-over");
        let line = "x".repeat(MAX_READ_LINE_BYTES + 1);
        workspace.write("f.txt", &format!("{line}\n"));
        let coding = belt(&workspace, Mode::ReadOnly);
        let output = act(
            &coding,
            ACTION_READ,
            json!({"path": "f.txt", "line_count": 1}),
        );
        assert_eq!(
            output.detail["content"].as_str().expect("a string").len(),
            MAX_READ_LINE_BYTES,
            "the line is cut to the cap"
        );
        assert_eq!(output.detail["truncated"], true);
    }

    #[test]
    fn a_read_of_exactly_the_line_cap_is_not_marked() {
        let workspace = Workspace::new("read-lines-exact");
        let body = "a\n".repeat(MAX_READ_LINES);
        workspace.write("f.txt", &body);
        let coding = belt(&workspace, Mode::ReadOnly);
        let output = act(
            &coding,
            ACTION_READ,
            json!({"path": "f.txt", "line_count": MAX_READ_LINES}),
        );
        assert_eq!(output.detail["lines"], MAX_READ_LINES);
        assert_eq!(output.detail["truncated"], false, "the whole file fit");
    }

    #[test]
    fn a_read_of_one_line_over_the_line_cap_is_marked() {
        let workspace = Workspace::new("read-lines-over");
        let body = "a\n".repeat(MAX_READ_LINES + 1);
        workspace.write("f.txt", &body);
        let coding = belt(&workspace, Mode::ReadOnly);
        let output = act(
            &coding,
            ACTION_READ,
            json!({"path": "f.txt", "line_count": MAX_READ_LINES}),
        );
        assert_eq!(output.detail["lines"], MAX_READ_LINES);
        assert_eq!(output.detail["truncated"], true, "one line was left unread");
        assert_eq!(output.detail["next_line"], MAX_READ_LINES + 1);
    }

    #[test]
    fn a_read_pages_from_start_line() {
        let workspace = Workspace::new("read-page");
        workspace.write("f.txt", "1\n2\n3\n4\n5\n");
        let coding = belt(&workspace, Mode::ReadOnly);
        let output = act(
            &coding,
            ACTION_READ,
            json!({"path": "f.txt", "start_line": 3, "line_count": 2}),
        );
        assert_eq!(output.detail["content"], "3\n4\n");
        assert_eq!(output.detail["lines"], 2);
        assert_eq!(output.detail["next_line"], 5);
    }

    #[test]
    fn a_read_of_a_missing_file_is_refused() {
        let workspace = Workspace::new("read-missing");
        let coding = belt(&workspace, Mode::ReadOnly);
        let output = act(&coding, ACTION_READ, json!({"path": "gone.txt"}));
        assert_eq!(output.kind, OutputKind::Error);
        assert_eq!(output.detail["reason"], "no file at `gone.txt`");
    }

    #[test]
    fn a_read_of_exactly_the_byte_budget_is_not_cut() {
        // The budget is what the content may spend after the envelope. A file whose
        // escaped content lands exactly on it is read whole; a mutant that compares
        // `>=` drops the last line and calls the detail truncated.
        let workspace = Workspace::new("read-budget");
        let lines = (MAX_READ_BYTES - READ_ENVELOPE_BYTES) / 12;
        assert_eq!(
            lines * 12,
            MAX_READ_BYTES - READ_ENVELOPE_BYTES,
            "the fixture's lines are exactly the budget"
        );
        workspace.write("f.txt", &"aaaaaaaaaa\n".repeat(lines));
        let coding = belt(&workspace, Mode::ReadOnly);
        let output = act(
            &coding,
            ACTION_READ,
            json!({"path": "f.txt", "line_count": lines}),
        );
        assert_eq!(output.detail["lines"], json!(lines), "the whole file fits");
        assert_eq!(output.detail["truncated"], false);
        assert_eq!(output.detail["next_line"], Value::Null);
    }

    #[test]
    fn a_final_line_of_exactly_the_window_without_a_newline_is_not_cut() {
        // The window is one byte wider than the line cap, so it fills exactly when
        // the final line is the cap and the input ends: nothing was dropped, and a
        // mutant that compares the filled window with `>=` reports a cut.
        let workspace = Workspace::new("read-window-eof");
        workspace.write("f.txt", &"x".repeat(MAX_READ_LINE_BYTES));
        let coding = belt(&workspace, Mode::ReadOnly);
        let output = act(&coding, ACTION_READ, json!({"path": "f.txt"}));
        assert_eq!(output.detail["lines"], 1);
        assert_eq!(output.detail["truncated"], false, "nothing was dropped");
        assert_eq!(
            output.detail["content"].as_str().expect("a string").len(),
            MAX_READ_LINE_BYTES
        );
    }

    #[test]
    fn an_escaped_character_costs_what_json_spends_on_it() {
        // The budget is on the serialized detail, so the line's cost is its escaped
        // length: a newline or a quote costs two bytes, a control character the six
        // of `\u001f`, and anything else its own width.
        assert_eq!(escaped_len("a"), 1);
        assert_eq!(escaped_len("\n"), 2);
        assert_eq!(escaped_len("\""), 2);
        assert_eq!(escaped_len("\u{1f}"), 6);
        assert_eq!(escaped_len(" "), 1, "a space is not a control character");
    }

    #[test]
    fn a_read_of_a_null_byte_file_is_refused() {
        let workspace = Workspace::new("read-nul");
        workspace.write_bytes("f.bin", b"a\0b\n");
        let coding = belt(&workspace, Mode::ReadOnly);
        let output = act(&coding, ACTION_READ, json!({"path": "f.bin"}));
        assert_eq!(output.kind, OutputKind::Error);
        assert_eq!(output.detail["reason"], "`f.bin` is not text");
    }

    #[test]
    fn a_read_of_a_directory_is_refused() {
        let workspace = Workspace::new("read-dir");
        std::fs::create_dir_all(workspace.0.join("sub")).expect("creates the directory");
        let coding = belt(&workspace, Mode::ReadOnly);
        let output = act(&coding, ACTION_READ, json!({"path": "sub"}));
        assert_eq!(output.kind, OutputKind::Error);
        assert_eq!(output.detail["reason"], "`sub` is not a regular file");
    }

    #[test]
    fn a_line_after_a_line_too_long_to_keep_is_still_read() {
        // The discard after an over-long line must stop at its newline, and the long
        // line must not end the search: either would lose the lines that follow it.
        let workspace = Workspace::new("long-line-next");
        let long = "x".repeat(MAX_READ_LINE_BYTES + 10);
        workspace.write("f.txt", &format!("{long}\nneedle here\nafter\n"));
        let coding = belt(&workspace, Mode::ReadOnly);

        let search = act(&coding, ACTION_CODE_SEARCH, json!({"pattern": "needle"}));
        assert_eq!(
            search.detail["matches"][0]["line"], 2,
            "the line after the cut one is found and numbered"
        );
        assert_eq!(search.detail["matches"][0]["text"], "needle here");

        let read = act(
            &coding,
            ACTION_READ,
            json!({"path": "f.txt", "line_count": 3}),
        );
        assert_eq!(
            read.detail["lines"], 3,
            "the lines after the cut one were read"
        );
        assert_eq!(read.detail["truncated"], true, "the cut line is marked");
        let content = read.detail["content"].as_str().expect("a string");
        assert!(
            content.ends_with("needle here\nafter\n"),
            "the discard stopped at the newline"
        );
    }

    #[test]
    fn a_read_detail_stays_within_the_byte_bound() {
        let workspace = Workspace::new("read-bound");
        let body = "line of text\n".repeat(5_000);
        workspace.write("f.txt", &body);
        let coding = belt(&workspace, Mode::ReadOnly);
        let output = act(
            &coding,
            ACTION_READ,
            json!({"path": "f.txt", "line_count": MAX_READ_LINES}),
        );
        assert_eq!(output.detail["truncated"], true);
        let returned = output.detail["lines"].as_u64().expect("a count");
        assert_eq!(
            output.detail["next_line"],
            json!(returned + 1),
            "the window resumes at the line after the last one returned"
        );
        let serialized = serde_json::to_string(&output.detail).expect("serializes");
        assert!(
            serialized.len() <= MAX_READ_BYTES,
            "the detail is bounded, was {} bytes",
            serialized.len()
        );
    }

    #[test]
    fn a_code_search_reports_the_file_line_and_text() {
        let workspace = Workspace::new("search-basic");
        workspace.write("f.txt", "alpha\nneedle here\nbeta\n");
        let coding = belt(&workspace, Mode::ReadOnly);
        let output = act(&coding, ACTION_CODE_SEARCH, json!({"pattern": "needle"}));
        assert_eq!(output.kind, OutputKind::Done);
        assert_eq!(
            output.detail["matches"],
            json!([{"path": "f.txt", "line": 2, "text": "needle here"}])
        );
        assert_eq!(output.detail["files"], 1);
        assert_eq!(output.detail["truncated"], false);
    }

    #[test]
    fn a_code_search_skips_the_git_directory() {
        let workspace = Workspace::new("search-git");
        workspace.write(".git/secret.txt", "needle\n");
        let coding = belt(&workspace, Mode::ReadOnly);
        let output = act(&coding, ACTION_CODE_SEARCH, json!({"pattern": "needle"}));
        assert_eq!(output.detail["matches"], json!([]));
        assert_eq!(output.detail["truncated"], false);
    }

    #[test]
    fn a_code_search_skips_a_symlink() {
        let workspace = Workspace::new("search-symlink");
        let outside = std::env::temp_dir().join(format!(
            "agent-coding-outside-{}-{}",
            std::process::id(),
            "search"
        ));
        std::fs::write(&outside, "needle\n").expect("writes outside");
        std::os::unix::fs::symlink(&outside, workspace.0.join("link")).expect("makes the link");
        let coding = belt(&workspace, Mode::ReadOnly);
        let output = act(&coding, ACTION_CODE_SEARCH, json!({"pattern": "needle"}));
        assert_eq!(output.detail["matches"], json!([]));
        let _ = std::fs::remove_file(&outside);
    }

    #[test]
    fn a_code_search_skips_a_file_over_the_size_cap() {
        let workspace = Workspace::new("search-big");
        let mut body = Vec::with_capacity(MAX_SEARCH_FILE_BYTES + 1);
        body.extend_from_slice(b"needle");
        body.resize(MAX_SEARCH_FILE_BYTES + 1, b'x');
        workspace.write_bytes("big.bin", &body);
        let coding = belt(&workspace, Mode::ReadOnly);
        let output = act(&coding, ACTION_CODE_SEARCH, json!({"pattern": "needle"}));
        assert_eq!(output.detail["matches"], json!([]));
        assert_eq!(output.detail["skipped"], 1);
        assert_eq!(
            output.detail["truncated"], false,
            "a skipped file is not a cut search"
        );
    }

    #[test]
    fn an_empty_pattern_is_refused() {
        let workspace = Workspace::new("search-empty");
        let coding = belt(&workspace, Mode::ReadOnly);
        let output = act(&coding, ACTION_CODE_SEARCH, json!({"pattern": ""}));
        assert_eq!(output.kind, OutputKind::Error);
        assert_eq!(output.detail["reason"], "the pattern is empty");
    }

    #[test]
    fn a_code_search_marks_a_bounded_search_as_truncated() {
        let workspace = Workspace::new("search-bound");
        workspace.write("f.txt", &"needle\n".repeat(MAX_SEARCH_MATCHES + 1));
        let coding = belt(&workspace, Mode::ReadOnly);
        let output = act(&coding, ACTION_CODE_SEARCH, json!({"pattern": "needle"}));
        assert_eq!(
            output.detail["matches"].as_array().expect("an array").len(),
            MAX_SEARCH_MATCHES
        );
        assert_eq!(output.detail["truncated"], true);
    }

    #[test]
    fn a_code_search_finds_a_match_past_the_reported_line_cap() {
        // The whole windowed line is searched, so a match after the report cap is
        // still found; only the text that reaches the model is cut.
        let workspace = Workspace::new("search-past-cap");
        let line = format!("{}needle\n", "x".repeat(300));
        workspace.write("f.txt", &line);
        let coding = belt(&workspace, Mode::ReadOnly);
        let output = act(&coding, ACTION_CODE_SEARCH, json!({"pattern": "needle"}));
        let matches = output.detail["matches"].as_array().expect("an array");
        assert_eq!(matches.len(), 1, "the match past the cap is found");
        assert_eq!(matches[0]["line"], 1);
        assert_eq!(
            matches[0]["text"].as_str().expect("a string").len(),
            MAX_SEARCH_LINE_BYTES,
            "the reported text is cut to the cap"
        );
    }

    #[test]
    fn a_pattern_of_exactly_the_cap_is_searched_and_one_more_is_refused() {
        let workspace = Workspace::new("pattern-cap");
        workspace.write("f.txt", "needle\n");
        let coding = belt(&workspace, Mode::ReadOnly);
        let accepted = act(
            &coding,
            ACTION_CODE_SEARCH,
            json!({"pattern": "n".repeat(MAX_PATTERN_BYTES)}),
        );
        assert_eq!(accepted.kind, OutputKind::Done, "the cap itself is legal");

        let refused = act(
            &coding,
            ACTION_CODE_SEARCH,
            json!({"pattern": "n".repeat(MAX_PATTERN_BYTES + 1)}),
        );
        assert_eq!(refused.kind, OutputKind::Error);
        assert_eq!(refused.detail["reason"], "the pattern is too long");
    }

    #[test]
    fn the_match_cap_stops_the_walk_and_marks_it() {
        // Two files match and the first has far more matches than the cap, so a walk
        // that only stops when every bound is reached would open the second file.
        let workspace = Workspace::new("search-match-cap");
        workspace.write("a.txt", &"needle here\n".repeat(MAX_SEARCH_MATCHES + 100));
        workspace.write("b.txt", "needle here\n");
        let coding = belt(&workspace, Mode::ReadOnly);
        let output = act(&coding, ACTION_CODE_SEARCH, json!({"pattern": "needle"}));
        assert_eq!(
            output.detail["matches"].as_array().expect("an array").len(),
            MAX_SEARCH_MATCHES
        );
        assert_eq!(output.detail["files"], 1, "the second file is never opened");
        assert_eq!(output.detail["truncated"], true);
    }

    #[test]
    fn the_match_text_cap_stops_at_the_line_that_would_pass_it() {
        // The reported text accumulates: forty lines of the report cap plus a line of
        // 192 bytes lands exactly on the text cap and is kept, and the one-byte line
        // after it is not. A comparison that uses `>=` drops the 192-byte line, and
        // one that multiplies or adds wrongly keeps the last one.
        let workspace = Workspace::new("search-text-cap");
        let mut body = String::new();
        for _ in 0..40 {
            body.push_str(&"x".repeat(MAX_SEARCH_LINE_BYTES));
            body.push('\n');
        }
        body.push_str(&"x".repeat(192));
        body.push('\n');
        body.push_str("x\n");
        workspace.write("f.txt", &body);
        let coding = belt(&workspace, Mode::ReadOnly);
        let output = act(&coding, ACTION_CODE_SEARCH, json!({"pattern": "x"}));
        assert_eq!(
            output.detail["matches"].as_array().expect("an array").len(),
            41,
            "the match that lands exactly on the cap is kept and the next is not"
        );
        assert_eq!(output.detail["truncated"], true);
    }

    #[test]
    fn the_match_text_cap_stops_a_match_that_would_pass_it() {
        // The report cap keeps the last match that fits and refuses the one after it,
        // so a check that only stops when both of its bounds are reached keeps it.
        let workspace = Workspace::new("search-text-over");
        let mut body = String::new();
        for _ in 0..40 {
            body.push_str(&"x".repeat(MAX_SEARCH_LINE_BYTES));
            body.push('\n');
        }
        body.push_str(&"x".repeat(300));
        body.push('\n');
        workspace.write("f.txt", &body);
        let coding = belt(&workspace, Mode::ReadOnly);
        let output = act(&coding, ACTION_CODE_SEARCH, json!({"pattern": "x"}));
        assert_eq!(
            output.detail["matches"].as_array().expect("an array").len(),
            40,
            "the match that would pass the text cap is refused"
        );
        assert_eq!(output.detail["truncated"], true);
    }

    #[test]
    fn a_search_deeper_than_the_depth_cap_is_marked_and_the_shallower_one_is_found() {
        // The walk descends to the cap and stops there: a match at the cap is found,
        // a match one level below it is not, and the result says it was cut short.
        let workspace = Workspace::new("search-depth");
        let nested = |depth: usize| {
            let mut path = PathBuf::new();
            for level in 0..depth {
                path.push(format!("d{level}"));
            }
            path
        };
        let found = format!("{}/found.txt", nested(MAX_SEARCH_DEPTH).display());
        let lost = format!("{}/lost.txt", nested(MAX_SEARCH_DEPTH + 1).display());
        workspace.write(&found, "needle\n");
        workspace.write(&lost, "needle\n");
        let coding = belt(&workspace, Mode::ReadOnly);
        let output = act(&coding, ACTION_CODE_SEARCH, json!({"pattern": "needle"}));
        assert_eq!(
            output.detail["matches"],
            json!([{"path": found, "line": 1, "text": "needle"}]),
            "the match at the cap is found and the one below it is not"
        );
        assert_eq!(output.detail["truncated"], true);
    }

    #[test]
    fn a_search_includes_a_file_of_exactly_the_size_cap() {
        let workspace = Workspace::new("search-size-exact");
        workspace.write("big.txt", &"x".repeat(MAX_SEARCH_FILE_BYTES));
        let coding = belt(&workspace, Mode::ReadOnly);
        let output = act(&coding, ACTION_CODE_SEARCH, json!({"pattern": "x"}));
        assert_eq!(output.detail["skipped"], 0, "the cap itself is searched");
        assert_eq!(output.detail["files"], 1);
    }

    #[test]
    fn the_walk_stops_at_the_entry_cap_and_marks_it() {
        // The entry cap is what bounds a walk that never matches. Reaching it is the
        // only way to tell a counted walk from an uncounted one, which is why this
        // fixture is the largest in the suite.
        let workspace = Workspace::new("search-entry-cap");
        for index in 0..=MAX_SEARCH_FILES {
            workspace.write(&format!("f{index:05}.txt"), "");
        }
        let coding = belt(&workspace, Mode::ReadOnly);
        let output = act(&coding, ACTION_CODE_SEARCH, json!({"pattern": "needle"}));
        assert_eq!(
            output.detail["files"],
            json!(MAX_SEARCH_FILES - 1),
            "the walk stops at the cap, before the file it would open next"
        );
        assert_eq!(output.detail["truncated"], true);
    }

    #[test]
    fn a_code_search_is_deterministic_in_file_order() {
        let workspace = Workspace::new("search-order");
        workspace.write("b.txt", "needle\n");
        workspace.write("a.txt", "needle\n");
        let coding = belt(&workspace, Mode::ReadOnly);
        let output = act(&coding, ACTION_CODE_SEARCH, json!({"pattern": "needle"}));
        let matches = output.detail["matches"].as_array().expect("an array");
        assert_eq!(matches[0]["path"], "a.txt");
        assert_eq!(matches[1]["path"], "b.txt");
    }

    #[test]
    fn the_patch_arm_reports_the_files_and_reports_done() {
        let workspace = Workspace::new("patch-arm");
        workspace.write("f.txt", "hello\n");
        let coding = belt(&workspace, Mode::ReadWrite);
        let reports = Reports::default();
        let diff = "--- a/f.txt\n+++ b/f.txt\n@@ -1 +1 @@\n-hello\n+world\n";
        let output = coding.act(
            &Command {
                action: ACTION_PATCH.to_owned(),
                detail: json!({"diff": diff}),
            },
            &reports,
        );
        assert_eq!(output.kind, OutputKind::Done, "detail: {}", output.detail);
        assert_eq!(output.detail["files"], json!(["f.txt"]));
        assert_eq!(output.detail["files_changed"], 1);
        assert_eq!(reports.details(), vec![json!({"files": ["f.txt"]})]);
        assert_eq!(
            std::fs::read_to_string(workspace.0.join("f.txt")).expect("readable"),
            "world\n"
        );
    }

    #[test]
    fn the_patch_arm_refuses_a_bad_diff_without_reporting() {
        let workspace = Workspace::new("patch-arm-bad");
        let coding = belt(&workspace, Mode::ReadWrite);
        let reports = Reports::default();
        let output = coding.act(
            &Command {
                action: ACTION_PATCH.to_owned(),
                detail: json!({"diff": "not a diff"}),
            },
            &reports,
        );
        assert_eq!(output.kind, OutputKind::Error);
        assert_eq!(output.detail["reason"], "the diff is not a unified diff");
        assert!(
            reports.details().is_empty(),
            "a plan that is refused reports nothing"
        );
    }

    #[test]
    fn the_bounds_are_pinned() {
        assert_eq!(DEFAULT_TIMEOUT, Duration::from_secs(300));
        assert_eq!(MAX_READ_LINES, 2_000);
        assert_eq!(MAX_READ_BYTES, 8 * 1024);
        assert_eq!(MAX_READ_LINE_BYTES, 4 * 1024);
        assert_eq!(DEFAULT_READ_LINES, 200);
        assert_eq!(MAX_PATTERN_BYTES, 256);
        assert_eq!(MAX_SEARCH_FILE_BYTES, 1024 * 1024);
        assert_eq!(MAX_SEARCH_FILES, 10_000);
        assert_eq!(MAX_SEARCH_DEPTH, 32);
        assert_eq!(MAX_SEARCH_MATCHES, 200);
        assert_eq!(MAX_SEARCH_TEXT_BYTES, 8 * 1024);
        assert_eq!(MAX_SEARCH_LINE_BYTES, 200);
    }
}
