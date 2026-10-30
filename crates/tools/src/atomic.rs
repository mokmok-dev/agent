//! Replacing a file so that a reader sees the old bytes or the new ones.

use std::fs::{File, OpenOptions, Permissions};
use std::io::{ErrorKind, Write as _};
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::ToolError;

static NEXT: AtomicU64 = AtomicU64::new(0);

/// Writes beside the target, flushes it, renames it over the target, and
/// flushes the directory, so a crash leaves one whole version.
pub fn replace(
    path: &Path,
    bytes: &[u8],
    mode: Option<u32>,
) -> Result<(), ToolError> {
    let Some(parent) = path.parent() else {
        return Err(ToolError::invalid_args(format!(
            "{} has no directory",
            path.display()
        )));
    };
    let (file, temp) = create(parent)?;
    let written = fill(file, &temp, bytes, mode);
    if written.is_err() {
        let _ = std::fs::remove_file(&temp);
        return written;
    }
    std::fs::rename(&temp, path).map_err(|error| {
        let _ = std::fs::remove_file(&temp);
        ToolError::io(path, &error)
    })?;
    sync_directory(parent)
}

fn create(parent: &Path) -> Result<(File, PathBuf), ToolError> {
    create_from(parent, NEXT.fetch_add(1, Ordering::Relaxed))
}

/// Creates the first of the eight names that is free, counting from `first`.
fn create_from(
    parent: &Path,
    first: u64,
) -> Result<(File, PathBuf), ToolError> {
    let mut last = None;
    for attempt in 0..8 {
        let temp = parent.join(format!(
            ".agent-tool.{}.{}.tmp",
            std::process::id(),
            first.wrapping_add(attempt)
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)
        {
            Ok(file) => return Ok((file, temp)),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => last = Some(temp),
            Err(error) => return Err(ToolError::io(&temp, &error)),
        }
    }
    Err(ToolError::io(
        last.as_deref().unwrap_or(parent),
        &std::io::Error::from(ErrorKind::AlreadyExists),
    ))
}

fn fill(
    mut file: File,
    temp: &Path,
    bytes: &[u8],
    mode: Option<u32>,
) -> Result<(), ToolError> {
    file.write_all(bytes)
        .map_err(|error| ToolError::io(temp, &error))?;
    if let Some(mode) = mode {
        file.set_permissions(Permissions::from_mode(mode))
            .map_err(|error| ToolError::io(temp, &error))?;
    }
    file.sync_all().map_err(|error| ToolError::io(temp, &error))
}

fn sync_directory(directory: &Path) -> Result<(), ToolError> {
    File::open(directory)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| ToolError::io(directory, &error))
}

#[cfg(test)]
mod tests;
