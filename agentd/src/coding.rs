//! The agent's coding toolbelt: `read`, `code_search`, and `patch` over one
//! workspace, in one mode, with `shell` alongside them.
//!
//! A [`Coding`] belt is built from a workspace root, a [`Mode`], and an optional
//! absolute `git` path. It computes the tools it offers **once**, at construction:
//! [`Mode`] is the capability set the agent advertises to the model and to bus
//! clients, not a kernel guarantee. The session's sandbox is the boundary that
//! actually confines a child; see `docs/session/agent.md`. A session the daemon
//! bound read-write stays writable at the kernel, so a read-only belt is a
//! contract, not a wall.
//!
//! Every read of a file or a child is bounded **while it is read**: `read` and
//! `code_search` window each line and stop on byte and count budgets, and the
//! `shell` and `patch` arms go through [`crate::child`], whose output cap is the
//! one the OOM incident established.
//!
//! `patch` checks the diff's syntax here, resolves and refuses every path against
//! the workspace root, reports the files it is about to touch, and only then hands
//! the diff to `git apply` on standard input. `git apply` validates the whole patch
//! before writing anything, so there is no separate dry run.

use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Read};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};

use crate::child;
use crate::contract::{Command, Output, OutputKind};
use crate::diff;
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

/// The cap on what one `git apply` run may emit.
const MAX_PATCH_OUTPUT_BYTES: usize = 4 * 1024;

/// The capability contract a session runs under.
///
/// A belt offers its tools once, so the model's tool list is exactly the actions
/// the belt can dispatch: a schema can never name an action it would refuse.
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
/// workspace, gated by one [`Mode`].
#[derive(Debug)]
pub struct Coding {
    /// The canonical workspace root every path is resolved against.
    root: PathBuf,
    /// The capability set the session runs under.
    mode: Mode,
    /// The absolute `git` a `patch` is applied with, when the host has one.
    git: Option<PathBuf>,
    /// How long a spawned child may run.
    timeout: Duration,
    /// The tools this belt offers, fixed at construction.
    offered: Vec<Spec>,
}

impl Coding {
    /// A belt over `root` in `mode`, applying patches with `git`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Root`] when `root` cannot be canonicalized as a directory.
    pub fn new(
        root: impl Into<PathBuf>,
        mode: Mode,
        git: Option<PathBuf>,
    ) -> Result<Self, Error> {
        let root = std::fs::canonicalize(root.into()).map_err(Error::Root)?;
        if !root.is_dir() {
            return Err(Error::Root(std::io::Error::new(
                std::io::ErrorKind::NotADirectory,
                "the workspace root is not a directory",
            )));
        }
        let offered = Spec::ALL
            .iter()
            .copied()
            .filter(|spec| mode.covers(spec.required()) && (*spec != Spec::Patch || git.is_some()))
            .collect();
        Ok(Self {
            root,
            mode,
            git,
            timeout: DEFAULT_TIMEOUT,
            offered,
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

    /// The mode the belt runs under.
    #[must_use]
    pub const fn mode(&self) -> Mode {
        self.mode
    }

    /// The tools this session offers the model, in [`Spec::ALL`] order.
    #[must_use]
    pub fn tools(&self) -> Vec<Tool> {
        self.offered
            .iter()
            .map(|spec| Tool {
                name: spec.action().to_owned(),
                description: spec.description().to_owned(),
                parameters: spec.parameters(),
            })
            .collect()
    }

    /// What the agent tells the model about itself, for the offered set.
    #[must_use]
    pub fn system_prompt(&self) -> String {
        let names = self
            .offered
            .iter()
            .map(|spec| spec.action())
            .collect::<Vec<_>>()
            .join(", ");
        let clause = if self.mode.is_read_only() {
            "This session is read-only, so no tool can change the workspace.".to_owned()
        } else {
            // The clause names the writing tools this belt actually offers, so it
            // stays true on a host the `patch` tool was withheld from.
            let writers = self
                .offered
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

    /// Whether the belt offers `action`.
    pub(crate) fn offers(
        &self,
        action: &str,
    ) -> bool {
        Spec::from_action(action).is_some_and(|spec| self.offered.contains(&spec))
    }
}

impl Capability for Coding {
    fn act(
        &self,
        command: &Command,
        reporter: &dyn Reporter,
    ) -> Output {
        let Some(spec) = Spec::from_action(&command.action) else {
            return refusal(
                &command.action,
                "the agent has no capability for this action",
            );
        };
        if !self.offered.contains(&spec) {
            let reason = if self.mode.covers(spec.required()) {
                "`patch` needs `git`, which this host does not have".to_owned()
            } else {
                format!(
                    "`{}` is not available in {} mode",
                    spec.action(),
                    self.mode.name()
                )
            };
            return refusal(&command.action, &reason);
        }
        spec.run(self, &command.detail, reporter)
    }
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
    let Ok(metadata) = std::fs::metadata(path) else {
        return;
    };
    if !metadata.is_file() {
        return;
    }
    if metadata.len() > MAX_SEARCH_FILE_BYTES as u64 {
        ctx.skipped += 1;
        return;
    }
    let Ok(file) = std::fs::File::open(path) else {
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

/// Apply a unified diff to the workspace through `git apply`.
fn patch(
    coding: &Coding,
    detail: &Value,
    reporter: &dyn Reporter,
) -> Output {
    let Some(source) = detail.get("diff").and_then(Value::as_str) else {
        return refusal(ACTION_PATCH, "malformed patch");
    };
    let parsed = match diff::parse(source) {
        Ok(parsed) => parsed,
        Err(error) => return refusal(ACTION_PATCH, error.reason()),
    };

    // Resolve every path before spawning anything: a patch is all-or-nothing, and
    // a refusal must not leave a partial change behind.
    let mut paths: Vec<String> = Vec::new();
    for file in &parsed.files {
        for path in [file.old.as_deref(), file.new.as_deref()]
            .into_iter()
            .flatten()
        {
            if let Err(error) = resolve_for_write(coding, path) {
                return refusal(ACTION_PATCH, error.reason());
            }
            // A modification names the same path on both sides; report it once.
            if !paths.iter().any(|existing| existing == path) {
                paths.push(path.to_owned());
            }
        }
    }

    let Some(git) = coding.git.as_deref() else {
        return refusal(
            ACTION_PATCH,
            "`patch` needs `git`, which this host does not have",
        );
    };
    reporter.report(json!({"files": &paths}));

    // `GIT_CEILING_DIRECTORIES` is the workspace's parent, so `git apply` does not
    // search upward for a repository outside the workspace.
    let ceiling = coding
        .root
        .parent()
        .map(|parent| parent.to_string_lossy().into_owned())
        .unwrap_or_default();
    let env = [
        ("GIT_CONFIG_NOSYSTEM", "1".to_owned()),
        ("GIT_CEILING_DIRECTORIES", ceiling),
    ];
    let strip = format!("-p{}", parsed.strip);
    let outcome = match child::run_child(
        git,
        &["apply", "--whitespace=nowarn", strip.as_str(), "-"],
        &coding.root,
        &env,
        Some(source.as_bytes()),
        coding.timeout,
        MAX_PATCH_OUTPUT_BYTES,
    ) {
        Ok(outcome) => outcome,
        Err(error) => return refusal(ACTION_PATCH, &format!("could not start `git`: {error}")),
    };
    if outcome.timed_out {
        return refusal(ACTION_PATCH, "the patch did not finish in time");
    }
    if outcome.code == Some(0) {
        Output {
            kind: OutputKind::Done,
            action: ACTION_PATCH.to_owned(),
            detail: json!({"files": paths, "strip": parsed.strip, "code": 0}),
        }
    } else {
        Output {
            kind: OutputKind::Error,
            action: ACTION_PATCH.to_owned(),
            detail: json!({
                "reason": "`git apply` refused the patch",
                "code": outcome.code,
                "stderr": outcome.stderr.text(),
            }),
        }
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
enum PathError {
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
    const fn reason(&self) -> &'static str {
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
fn resolve_for_write(
    coding: &Coding,
    raw: &str,
) -> Result<WorkspacePath, PathError> {
    let resolved = resolve(coding, raw)?;
    if Path::new(raw)
        .components()
        .any(|component| matches!(component, Component::Normal(part) if part == OsStr::new(".git")))
    {
        return Err(PathError::Protected);
    }
    Ok(resolved)
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
    let read = window.read_until(b'\n', &mut buffer)?;
    if read == 0 {
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

    /// Locate a tool by name on the host, the way a shell would. A child's
    /// environment is cleared, so a tool is named by absolute path.
    fn helper_path(name: &str) -> Option<PathBuf> {
        std::env::var("PATH")
            .unwrap_or_default()
            .split(':')
            .filter(|dir| !dir.is_empty())
            .map(|dir| Path::new(dir).join(name))
            .find(|candidate| candidate.is_file())
    }

    /// A belt over `workspace`, with the host's `git`.
    fn belt(
        workspace: &Workspace,
        mode: Mode,
    ) -> Coding {
        Coding::new(&workspace.0, mode, helper_path("git")).expect("the workspace opens")
    }

    /// A belt over `workspace`, with no `git`.
    fn belt_without_git(
        workspace: &Workspace,
        mode: Mode,
    ) -> Coding {
        Coding::new(&workspace.0, mode, None).expect("the workspace opens")
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
    fn a_readwrite_belt_with_git_offers_all_four() {
        let workspace = Workspace::new("rw-tools");
        let coding = belt(&workspace, Mode::ReadWrite);
        assert_eq!(
            tool_names(&coding),
            ["read", "code_search", "patch", "shell"]
        );
    }

    #[test]
    fn a_readwrite_belt_without_git_omits_patch() {
        let workspace = Workspace::new("rw-nogit");
        let coding = belt_without_git(&workspace, Mode::ReadWrite);
        assert_eq!(tool_names(&coding), ["read", "code_search", "shell"]);
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
    fn a_readwrite_belt_without_git_refuses_patch() {
        let workspace = Workspace::new("rw-nogit-patch");
        let coding = belt_without_git(&workspace, Mode::ReadWrite);
        let output = act(&coding, ACTION_PATCH, json!({"diff": "anything"}));
        assert_eq!(output.kind, OutputKind::Error);
        assert_eq!(
            output.detail["reason"],
            "`patch` needs `git`, which this host does not have"
        );
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
        let coding = belt_without_git(&workspace, Mode::ReadOnly);
        let prompt = coding.system_prompt();
        assert!(prompt.contains("read, code_search"), "prompt was: {prompt}");
        assert!(
            prompt.contains("read-only"),
            "the read-only clause is stated: {prompt}"
        );
    }

    #[test]
    fn a_readwrite_prompt_names_only_the_writers_the_belt_offers() {
        let workspace = Workspace::new("prompt-writers");
        let with_git = belt(&workspace, Mode::ReadWrite).system_prompt();
        assert!(
            with_git.contains("through `patch` and `shell`"),
            "both writers are named: {with_git}"
        );

        let without_git = belt_without_git(&workspace, Mode::ReadWrite).system_prompt();
        assert!(
            without_git.contains("through `shell`"),
            "the writer this belt has is named: {without_git}"
        );
        assert!(
            !without_git.contains("`patch`"),
            "a belt with no `git` must not claim the patch tool: {without_git}"
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
    fn a_patch_applies_a_git_style_diff() {
        let workspace = Workspace::new("patch-git");
        workspace.write("f.txt", "hello\n");
        let coding = belt(&workspace, Mode::ReadWrite);
        let diff = "--- a/f.txt\n+++ b/f.txt\n@@ -1 +1 @@\n-hello\n+world\n";
        let output = act(&coding, ACTION_PATCH, json!({"diff": diff}));
        assert_eq!(output.kind, OutputKind::Done, "detail: {}", output.detail);
        assert_eq!(output.detail["strip"], 1);
        assert_eq!(output.detail["code"], 0);
        assert_eq!(output.detail["files"], json!(["f.txt"]));
        assert_eq!(
            std::fs::read_to_string(workspace.0.join("f.txt")).expect("readable"),
            "world\n"
        );
    }

    #[test]
    fn a_patch_applies_an_unprefixed_nested_diff() {
        let workspace = Workspace::new("patch-nested");
        workspace.write("sub/g.txt", "old\n");
        let coding = belt(&workspace, Mode::ReadWrite);
        let diff = "--- sub/g.txt\n+++ sub/g.txt\n@@ -1 +1 @@\n-old\n+new\n";
        let output = act(&coding, ACTION_PATCH, json!({"diff": diff}));
        assert_eq!(output.kind, OutputKind::Done, "detail: {}", output.detail);
        assert_eq!(output.detail["strip"], 0);
        assert_eq!(
            std::fs::read_to_string(workspace.0.join("sub/g.txt")).expect("readable"),
            "new\n"
        );
    }

    #[test]
    fn a_patch_reports_the_files_before_applying() {
        let workspace = Workspace::new("patch-report");
        workspace.write("f.txt", "hello\n");
        let coding = belt(&workspace, Mode::ReadWrite);
        let reports = Reports::default();
        let diff = "--- a/f.txt\n+++ b/f.txt\n@@ -1 +1 @@\n-hello\n+world\n";
        coding.act(
            &Command {
                action: ACTION_PATCH.to_owned(),
                detail: json!({"diff": diff}),
            },
            &reports,
        );
        assert_eq!(reports.details(), vec![json!({"files": ["f.txt"]})]);
    }

    #[test]
    fn a_patch_refuses_a_lying_hunk_count_before_spawning() {
        let workspace = Workspace::new("patch-lying");
        workspace.write("f.txt", "a\n");
        let coding = belt(&workspace, Mode::ReadWrite);
        let reports = Reports::default();
        let diff = "--- f.txt\n+++ f.txt\n@@ -1,2 +1,2 @@\n a\n";
        let output = coding.act(
            &Command {
                action: ACTION_PATCH.to_owned(),
                detail: json!({"diff": diff}),
            },
            &reports,
        );
        assert_eq!(output.kind, OutputKind::Error);
        assert_eq!(
            output.detail["reason"],
            "a hunk header does not match its lines"
        );
        assert!(reports.details().is_empty(), "nothing was spawned");
        assert_eq!(
            std::fs::read_to_string(workspace.0.join("f.txt")).expect("readable"),
            "a\n",
            "the file is untouched"
        );
    }

    #[test]
    fn a_patch_that_escapes_the_workspace_is_refused() {
        let workspace = Workspace::new("patch-escape");
        let coding = belt(&workspace, Mode::ReadWrite);
        let diff = "--- ../escaped.txt\n+++ ../escaped.txt\n@@ -1 +1 @@\n-a\n+b\n";
        let output = act(&coding, ACTION_PATCH, json!({"diff": diff}));
        assert_eq!(output.kind, OutputKind::Error);
        assert_eq!(
            output.detail["reason"],
            "the path contains a parent component"
        );
    }

    #[test]
    fn a_patch_git_refuses_reports_its_bounded_stderr() {
        let workspace = Workspace::new("patch-refused");
        let coding = belt(&workspace, Mode::ReadWrite);
        let diff = "--- a/missing.txt\n+++ b/missing.txt\n@@ -1 +1 @@\n-x\n+y\n";
        let output = act(&coding, ACTION_PATCH, json!({"diff": diff}));
        assert_eq!(output.kind, OutputKind::Error);
        assert_eq!(output.detail["reason"], "`git apply` refused the patch");
        assert_ne!(output.detail["code"], 0);
        assert!(
            output.detail["stderr"]
                .as_str()
                .is_some_and(|stderr| !stderr.is_empty()),
            "git's stderr is reported: {}",
            output.detail
        );
    }

    #[test]
    fn an_already_applied_patch_fails() {
        let workspace = Workspace::new("patch-applied");
        workspace.write("f.txt", "hello\n");
        let coding = belt(&workspace, Mode::ReadWrite);
        let diff = "--- a/f.txt\n+++ b/f.txt\n@@ -1 +1 @@\n-hello\n+world\n";
        let first = act(&coding, ACTION_PATCH, json!({"diff": diff}));
        assert_eq!(first.kind, OutputKind::Done);
        let second = act(&coding, ACTION_PATCH, json!({"diff": diff}));
        assert_eq!(second.kind, OutputKind::Error);
        assert_eq!(second.detail["reason"], "`git apply` refused the patch");
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
        assert_eq!(MAX_PATCH_OUTPUT_BYTES, 4 * 1024);
    }
}
