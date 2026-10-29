//! Writing a file in the workspace.

use std::os::unix::fs::PermissionsExt as _;

use agent_core::event::Risk;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::atomic;
use crate::ctx::Ctx;
use crate::error::ToolError;
use crate::{Class, Limits, Scope, Tool};

/// The most content one call may write.
const CONTENT_BYTES: usize = 16 * 1024 * 1024;

/// Writes a file in the workspace.
#[derive(Debug, Clone, Copy)]
pub struct WriteFile;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Args {
    /// The file, relative to the workspace. Its parents are created.
    pub path: String,
    /// The whole new content.
    pub content: String,
}

#[derive(Debug, Serialize)]
pub struct Output {
    pub path: String,
    pub bytes: usize,
}

impl Tool for WriteFile {
    type Args = Args;
    type Output = Output;

    const NAME: &'static str = "write_file";
    const DESCRIPTION: &'static str = "Write a whole file in the workspace, creating its parents. \
        The file is replaced atomically; prefer edit_file for a change to a file that exists.";
    const CLASS: Class = Class::Idempotent;
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
        let bytes = args.content.as_bytes();
        if bytes.len() > CONTENT_BYTES {
            return Err(ToolError::too_large(format!(
                "{} is {} bytes; the limit is {CONTENT_BYTES}",
                args.path,
                bytes.len()
            )));
        }
        let target = ctx.workspace().writable(&args.path)?;
        let Some(name) = target.file_name() else {
            return Err(ToolError::not_a_file(format!(
                "{} names no file",
                args.path
            )));
        };
        let Some(parent) = target.parent() else {
            return Err(ToolError::not_a_file(format!(
                "{} has no directory",
                args.path
            )));
        };
        std::fs::create_dir_all(parent).map_err(|error| ToolError::io(parent, &error))?;
        let parent =
            std::fs::canonicalize(parent).map_err(|error| ToolError::io(parent, &error))?;
        if !parent.starts_with(ctx.workspace().root()) {
            return Err(ToolError::outside_workspace(format!(
                "{} resolves outside the workspace",
                args.path
            )));
        }
        let target = parent.join(name);
        let existing = std::fs::symlink_metadata(&target).ok();
        if existing.as_ref().is_some_and(std::fs::Metadata::is_dir) {
            return Err(ToolError::not_a_file(format!(
                "{} is a directory",
                args.path
            )));
        }
        let mode = existing.map_or(0o644, |metadata| metadata.permissions().mode() & 0o7777);
        atomic::replace(&target, bytes, Some(mode))?;
        Ok(Output {
            path: args.path,
            bytes: bytes.len(),
        })
    }
}
