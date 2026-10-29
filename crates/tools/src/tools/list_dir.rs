//! Listing a directory tree without following symlinks.

use std::path::Path;

use agent_core::event::Risk;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ctx::Ctx;
use crate::envelope::fit;
use crate::error::ToolError;
use crate::{Class, Limits, Scope, Tool};

/// The most entries one answer may carry.
const ENTRIES: usize = 1000;

/// The most bytes of JSON one answer may carry.
const JSON_BYTES: usize = 24 * 1024;

/// The deepest a listing may go.
const DEPTHS: u8 = 3;

/// Lists a directory in the workspace.
#[derive(Debug, Clone, Copy)]
pub struct ListDir;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Args {
    /// The directory, relative to the workspace. Defaults to the workspace root.
    pub path: Option<String>,
    /// How many levels to list, from 1 to 3. Defaults to 1.
    pub depth: Option<u8>,
}

#[derive(Debug, Serialize)]
pub struct Output {
    pub path: String,
    pub entries: Vec<Entry>,
    /// Whether the tree holds more than this answer returned.
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Entry {
    /// Relative to the workspace.
    pub path: String,
    pub kind: Kind,
    /// The byte length of a regular file.
    pub size: Option<u64>,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    File,
    Dir,
    Symlink,
    Other,
}

impl Tool for ListDir {
    type Args = Args;
    type Output = Output;

    const NAME: &'static str = "list_dir";
    const DESCRIPTION: &'static str = "List a directory in the workspace, one to three levels \
        deep. Entries are sorted by path; a symlink is reported and never followed.";
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
        let relative = args.path.as_deref().unwrap_or(".");
        let root = ctx.workspace().existing(relative)?;
        if !root.is_dir() {
            return Err(ToolError::not_a_file(format!(
                "{relative} is not a directory"
            )));
        }
        let depth = args.depth.unwrap_or(1);
        if !(1..=DEPTHS).contains(&depth) {
            return Err(ToolError::invalid_args(format!(
                "depth must be from 1 to {DEPTHS}"
            )));
        }

        let mut entries = Vec::new();
        let mut pending = vec![(root, 1_u8)];
        let mut stopped = false;
        while let Some((directory, level)) = pending.pop() {
            let read =
                std::fs::read_dir(&directory).map_err(|error| ToolError::io(&directory, &error))?;
            for item in read {
                let item = item.map_err(|error| ToolError::io(&directory, &error))?;
                let path = item.path();
                let kind = kind_of(&path)?;
                entries.push(Entry {
                    path: ctx.workspace().relative(&path),
                    kind,
                    size: size_of(&path, kind),
                });
                if entries.len() > ENTRIES {
                    stopped = true;
                    break;
                }
                if matches!(kind, Kind::Dir) && level < depth && item.file_name() != ".git" {
                    pending.push((path, level + 1));
                }
            }
            if stopped {
                break;
            }
        }
        entries.sort_unstable_by(|left, right| left.path.cmp(&right.path));
        entries.truncate(ENTRIES);

        Ok(fit(&entries, JSON_BYTES, |kept, over| Output {
            path: relative.to_string(),
            entries: kept.to_vec(),
            truncated: stopped || over,
        }))
    }
}

fn kind_of(path: &Path) -> Result<Kind, ToolError> {
    let metadata = std::fs::symlink_metadata(path).map_err(|error| ToolError::io(path, &error))?;
    let kind = metadata.file_type();
    Ok(if kind.is_symlink() {
        Kind::Symlink
    } else if kind.is_dir() {
        Kind::Dir
    } else if kind.is_file() {
        Kind::File
    } else {
        Kind::Other
    })
}

fn size_of(
    path: &Path,
    kind: Kind,
) -> Option<u64> {
    matches!(kind, Kind::File)
        .then(|| std::fs::metadata(path).ok().map(|metadata| metadata.len()))
        .flatten()
}
