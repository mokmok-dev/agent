//! Searching workspace files with a regular expression.

use std::collections::VecDeque;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use agent_core::event::Risk;
use globset::{GlobBuilder, GlobMatcher};
use regex::{Regex, RegexBuilder};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ctx::Ctx;
use crate::envelope::fit;
use crate::error::ToolError;
use crate::path::Workspace;
use crate::{Class, Limits, Scope, Tool};

/// The most matches one answer returns.
const MATCHES: usize = 200;

/// The most bytes of JSON one answer may carry.
const JSON_BYTES: usize = 24 * 1024;

/// The largest file the walk reads.
const FILE_BYTES: usize = 1024 * 1024;

/// How far into a file the walk looks for a NUL byte.
const NUL_BYTES: usize = 8 * 1024;

/// The most of one line a match reports.
const TEXT_BYTES: usize = 500;

/// Searches the workspace with a regular expression.
#[derive(Debug, Clone, Copy)]
pub struct Search;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Args {
    /// A regular expression in Rust `regex` syntax.
    pub pattern: String,
    /// Where to start, relative to the workspace. Defaults to the workspace root.
    pub path: Option<String>,
    /// A glob over the workspace-relative path, with `*`, `?`, and `**`.
    pub glob: Option<String>,
    /// Whether the pattern is case sensitive. Defaults to true.
    pub case_sensitive: Option<bool>,
    /// The most matches to return. Defaults to 200 and cannot exceed it.
    pub max_results: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct Output {
    pub matches: Vec<Found>,
    /// Whether the search stopped at its limit before the tree was exhausted.
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Found {
    /// Relative to the workspace.
    pub path: String,
    pub line: usize,
    /// Counted in characters from one.
    pub column: usize,
    /// The whole line, cut to 500 bytes when it is longer.
    pub text: String,
}

impl Tool for Search {
    type Args = Args;
    type Output = Output;

    const NAME: &'static str = "search";
    const DESCRIPTION: &'static str = "Search the workspace for a Rust regular expression and \
        return each matching line with its path, line, and column. Binary and oversized files \
        are skipped.";
    const CLASS: Class = Class::Idempotent;
    const RISK: Risk = Risk::Read;
    const SCOPE: Scope = Scope::Read;
    const LIMITS: Limits = Limits {
        timeout_ms: 30_000,
        event_bytes: 64 * 1024,
    };
    const ENV: &'static [&'static str] = &[];

    fn run(
        args: Args,
        ctx: &Ctx<'_>,
    ) -> Result<Output, ToolError> {
        let pattern = pattern_of(&args)?;
        let glob = glob_of(&args)?;
        let scanner = Scanner {
            workspace: ctx.workspace(),
            glob: glob.as_ref(),
            pattern: &pattern,
            limit: limit_of(&args)?,
        };
        let relative = args.path.as_deref().unwrap_or(".");
        let start = ctx.workspace().existing(relative)?;
        let metadata = std::fs::metadata(&start).map_err(|error| ToolError::io(&start, &error))?;
        let mut matches = Vec::new();
        let stopped = if metadata.is_file() {
            scanner.file(&start, &mut matches)?
        } else if metadata.is_dir() {
            walk(start, &scanner, &mut matches)?
        } else {
            return Err(ToolError::not_a_file(format!(
                "{relative} is neither a file nor a directory"
            )));
        };

        Ok(fit(&matches, JSON_BYTES, |kept, over| Output {
            matches: kept.to_vec(),
            truncated: stopped || over,
        }))
    }
}

fn pattern_of(args: &Args) -> Result<Regex, ToolError> {
    RegexBuilder::new(&args.pattern)
        .case_insensitive(!args.case_sensitive.unwrap_or(true))
        .build()
        .map_err(|error| {
            ToolError::invalid_args(format!("the pattern is not a regular expression: {error}"))
        })
}

fn glob_of(args: &Args) -> Result<Option<GlobMatcher>, ToolError> {
    args.glob
        .as_deref()
        .map(|pattern| {
            GlobBuilder::new(pattern)
                .literal_separator(true)
                .build()
                .map(|glob| glob.compile_matcher())
        })
        .transpose()
        .map_err(|error| ToolError::invalid_args(format!("the glob is not usable: {error}")))
}

fn limit_of(args: &Args) -> Result<usize, ToolError> {
    match args.max_results {
        Some(0) => Err(ToolError::invalid_args("max_results must be at least 1")),
        Some(wanted) => Ok(wanted.min(MATCHES)),
        None => Ok(MATCHES),
    }
}

/// Walks the tree from `start`, adding matches, and reports whether the limit
/// was reached.
fn walk(
    start: PathBuf,
    scanner: &Scanner<'_>,
    matches: &mut Vec<Found>,
) -> Result<bool, ToolError> {
    let mut pending = VecDeque::from([start]);
    while let Some(directory) = pending.pop_front() {
        for child in children(&directory)? {
            if visit(child, &mut pending, scanner, matches)? {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// Handles one child: queues a directory, searches a file, and reports whether
/// the search limit was reached.
fn visit(
    child: PathBuf,
    pending: &mut VecDeque<PathBuf>,
    scanner: &Scanner<'_>,
    matches: &mut Vec<Found>,
) -> Result<bool, ToolError> {
    let kind = std::fs::symlink_metadata(&child)
        .map_err(|error| ToolError::io(&child, &error))?
        .file_type();
    if kind.is_symlink() {
        return Ok(false);
    }
    if kind.is_dir() {
        if !is_git(&child) {
            pending.push_back(child);
        }
        return Ok(false);
    }
    if kind.is_file() {
        return scanner.file(&child, matches);
    }
    Ok(false)
}

fn children(directory: &Path) -> Result<Vec<PathBuf>, ToolError> {
    let mut children: Vec<PathBuf> = std::fs::read_dir(directory)
        .map_err(|error| ToolError::io(directory, &error))?
        .map(|item| {
            item.map(|item| item.path())
                .map_err(|error| ToolError::io(directory, &error))
        })
        .collect::<Result<_, _>>()?;
    children.sort_unstable();
    Ok(children)
}

fn is_git(directory: &Path) -> bool {
    directory.file_name().is_some_and(|name| name == ".git")
}

struct Scanner<'a> {
    workspace: &'a Workspace,
    glob: Option<&'a GlobMatcher>,
    pattern: &'a Regex,
    limit: usize,
}

impl Scanner<'_> {
    /// Adds the file's matches, and reports whether the limit was reached.
    fn file(
        &self,
        path: &Path,
        matches: &mut Vec<Found>,
    ) -> Result<bool, ToolError> {
        let relative = self.workspace.relative(path);
        if self.glob.is_some_and(|glob| !glob.is_match(&relative)) {
            return Ok(false);
        }
        let Some(text) = readable(path)? else {
            return Ok(false);
        };
        for (index, line) in text.lines().enumerate() {
            for found in self.pattern.find_iter(line) {
                matches.push(Found {
                    path: relative.clone(),
                    line: index + 1,
                    column: line[..found.start()].chars().count() + 1,
                    text: cut(line),
                });
                if matches.len() >= self.limit {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }
}

/// The file's text, or nothing when the walk must skip it.
fn readable(path: &Path) -> Result<Option<String>, ToolError> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .and_then(|file| file.take(FILE_BYTES as u64 + 1).read_to_end(&mut bytes))
        .map_err(|error| ToolError::io(path, &error))?;
    if bytes.len() > FILE_BYTES || bytes[..bytes.len().min(NUL_BYTES)].contains(&0) {
        return Ok(None);
    }
    let Ok(text) = String::from_utf8(bytes) else {
        return Ok(None);
    };
    Ok(Some(text))
}

fn cut(line: &str) -> String {
    if line.len() <= TEXT_BYTES {
        return line.to_owned();
    }
    let mut end = TEXT_BYTES;
    for _ in 0..=TEXT_BYTES {
        if line.is_char_boundary(end) {
            break;
        }
        end -= 1;
    }
    format!("{}...", &line[..end])
}
