//! Replacing one exact string in a file.

use std::os::unix::fs::PermissionsExt as _;

use agent_core::event::Risk;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::atomic;
use crate::ctx::Ctx;
use crate::error::ToolError;
use crate::{Class, Limits, Scope, Tool};

/// The largest file one call may edit.
const FILE_BYTES: usize = 16 * 1024 * 1024;

/// Replaces one exact string in a file in the workspace.
#[derive(Debug, Clone, Copy)]
pub struct EditFile;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Args {
    /// The file, relative to the workspace.
    pub path: String,
    /// The exact text to replace. It must occur exactly once unless `all` is true.
    pub old: String,
    /// The text that takes its place.
    pub new: String,
    /// Replace every occurrence instead of requiring exactly one.
    pub all: Option<bool>,
}

#[derive(Debug, Serialize)]
pub struct Output {
    pub path: String,
    pub replacements: usize,
    pub bytes: usize,
}

impl Tool for EditFile {
    type Args = Args;
    type Output = Output;

    const NAME: &'static str = "edit_file";
    const DESCRIPTION: &'static str = "Replace one exact string in a UTF-8 file in the workspace. \
        Fails as not_found when the string is absent and as ambiguous when it occurs more than \
        once unless all is true.";
    const CLASS: Class = Class::NonIdempotent;
    const RISK: Risk = Risk::Write;
    const SCOPE: Scope = Scope::Workspace;
    const LIMITS: Limits = Limits {
        timeout_ms: 15_000,
        event_bytes: 4 * 1024,
    };
    const ENV: &'static [&'static str] = &[];

    fn run(
        args: Args,
        ctx: &Ctx<'_>,
    ) -> Result<Output, ToolError> {
        if args.old.is_empty() {
            return Err(ToolError::invalid_args("old must not be empty"));
        }
        let path = ctx.workspace().existing(&args.path)?;
        let metadata = std::fs::metadata(&path).map_err(|error| ToolError::io(&path, &error))?;
        if !metadata.is_file() {
            return Err(ToolError::not_a_file(format!(
                "{} is not a file",
                args.path
            )));
        }
        if metadata.len() > FILE_BYTES as u64 {
            return Err(ToolError::too_large(format!(
                "{} is {} bytes; the limit is {FILE_BYTES}",
                args.path,
                metadata.len()
            )));
        }
        let bytes = std::fs::read(&path).map_err(|error| ToolError::io(&path, &error))?;
        let text = std::str::from_utf8(&bytes)
            .map_err(|_| ToolError::not_text(format!("{} is not UTF-8 text", args.path)))?;
        let occurrences = text.matches(&args.old).count();
        let all = args.all.unwrap_or(false);
        if occurrences == 0 {
            return Err(ToolError::not_found(format!(
                "{} does not contain the text to replace",
                args.path
            )));
        }
        if occurrences > 1 && !all {
            return Err(ToolError::ambiguous(format!(
                "{} contains the text {occurrences} times; pass all, or a longer old",
                args.path
            )));
        }
        let replacements = if all { occurrences } else { 1 };
        let next = text.replacen(&args.old, &args.new, replacements);
        let mode = metadata.permissions().mode() & 0o7777;
        atomic::replace(&path, next.as_bytes(), Some(mode))?;
        Ok(Output {
            path: args.path,
            replacements,
            bytes: next.len(),
        })
    }
}
