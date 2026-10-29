//! The one tree every tool path is resolved against.

use std::ffi::OsStr;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

use crate::error::ToolError;

/// A canonical directory that every tool path is resolved inside.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    root: PathBuf,
}

impl Workspace {
    /// Canonicalizes the directory a tool works in.
    ///
    /// # Errors
    ///
    /// Returns `not_found` when it is absent, and `not_a_file` when it is not a
    /// directory.
    pub fn open(directory: &Path) -> Result<Self, ToolError> {
        let root = std::fs::canonicalize(directory).map_err(|error| {
            if error.kind() == ErrorKind::NotFound {
                ToolError::not_found(format!(
                    "the workspace {} does not exist",
                    directory.display()
                ))
            } else {
                ToolError::io(directory, &error)
            }
        })?;
        if !root.is_dir() {
            return Err(ToolError::not_a_file(format!(
                "the workspace {} is not a directory",
                root.display()
            )));
        }
        Ok(Self { root })
    }

    /// The canonical root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolves a workspace-relative path that already exists.
    ///
    /// # Errors
    ///
    /// Refuses an absolute path and any `..` component, and refuses a path
    /// whose canonical form leaves the workspace after following symlinks.
    pub fn existing(
        &self,
        relative: &str,
    ) -> Result<PathBuf, ToolError> {
        let joined = self.join(relative)?;
        let canonical = std::fs::canonicalize(&joined).map_err(|error| {
            if error.kind() == ErrorKind::NotFound {
                ToolError::not_found(format!("{relative} does not exist"))
            } else {
                ToolError::io(&joined, &error)
            }
        })?;
        self.inside(relative, canonical)
    }

    /// Resolves a path a tool may create or replace.
    ///
    /// The deepest part that exists is canonicalized and checked, so a symlink
    /// that leaves the workspace is refused even when the leaf does not exist
    /// yet. Nothing is created here.
    ///
    /// # Errors
    ///
    /// The same refusals as [`Self::existing`], and `io` when an existing part
    /// cannot be canonicalized.
    pub fn writable(
        &self,
        relative: &str,
    ) -> Result<PathBuf, ToolError> {
        let parts = Self::parts(relative)?;
        let mut existing = self.root.clone();
        let mut missing: Vec<&OsStr> = Vec::new();
        for part in parts {
            let candidate = existing.join(part);
            if missing.is_empty() && std::fs::symlink_metadata(&candidate).is_ok() {
                existing = candidate;
            } else {
                missing.push(part);
            }
        }
        let canonical = self.inside(
            relative,
            std::fs::canonicalize(&existing).map_err(|error| ToolError::io(&existing, &error))?,
        )?;
        Ok(missing.iter().fold(canonical, |path, part| path.join(part)))
    }

    /// A path as the model sees it, relative to the workspace.
    #[must_use]
    pub fn relative(
        &self,
        path: &Path,
    ) -> String {
        path.strip_prefix(&self.root).map_or_else(
            |_| path.to_string_lossy().into_owned(),
            |rest| rest.to_string_lossy().into_owned(),
        )
    }

    fn join(
        &self,
        relative: &str,
    ) -> Result<PathBuf, ToolError> {
        let parts = Self::parts(relative)?;
        Ok(parts
            .iter()
            .fold(self.root.clone(), |path, part| path.join(part)))
    }

    fn parts(relative: &str) -> Result<Vec<&OsStr>, ToolError> {
        if relative.is_empty() {
            return Err(ToolError::invalid_args("path is empty"));
        }
        let mut parts = Vec::new();
        for component in Path::new(relative).components() {
            match component {
                Component::Normal(part) => parts.push(part),
                Component::CurDir => {},
                Component::ParentDir => {
                    return Err(ToolError::outside_workspace(format!(
                        "{relative} climbs out of the workspace"
                    )));
                },
                Component::RootDir | Component::Prefix(_) => {
                    return Err(ToolError::outside_workspace(format!(
                        "{relative} is an absolute path"
                    )));
                },
            }
        }
        Ok(parts)
    }

    fn inside(
        &self,
        relative: &str,
        canonical: PathBuf,
    ) -> Result<PathBuf, ToolError> {
        if canonical.starts_with(&self.root) {
            Ok(canonical)
        } else {
            Err(ToolError::outside_workspace(format!(
                "{relative} resolves outside the workspace"
            )))
        }
    }
}

#[cfg(test)]
mod tests;
