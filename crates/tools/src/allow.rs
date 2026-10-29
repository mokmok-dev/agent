//! The programs one invocation of `run` may start.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use crate::error::ToolError;

/// The programs a tool invocation permits.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Allowlist {
    entries: Vec<Entry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Entry {
    Name(OsString),
    Absolute(PathBuf),
}

impl Entry {
    fn resolve(
        &self,
        path: Option<&OsStr>,
    ) -> Option<PathBuf> {
        match self {
            Self::Name(name) => which(&name.to_string_lossy(), path),
            Self::Absolute(absolute) => executable(absolute),
        }
    }

    fn names(
        &self,
        program: &str,
    ) -> bool {
        matches!(self, Self::Name(name) if name.as_os_str() == OsStr::new(program))
    }
}

impl fmt::Display for Entry {
    fn fmt(
        &self,
        formatter: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        match self {
            Self::Name(name) => formatter.write_str(&name.to_string_lossy()),
            Self::Absolute(path) => write!(formatter, "{}", path.display()),
        }
    }
}

/// Why the program list on the command line was not usable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AllowError {
    /// An entry would be resolved against the workspace rather than the `PATH`.
    Relative(String),
    /// An entry named nothing.
    Empty,
}

impl fmt::Display for AllowError {
    fn fmt(
        &self,
        formatter: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        match self {
            Self::Relative(entry) => write!(
                formatter,
                "the allowed program {entry} is a relative path; name the program or give an absolute path"
            ),
            Self::Empty => formatter.write_str("an allowed program is empty"),
        }
    }
}

impl std::error::Error for AllowError {}

impl Allowlist {
    /// Reads the repeated `--allow=<program>` values.
    ///
    /// # Errors
    ///
    /// Refuses an empty entry, and a relative entry containing a slash, because
    /// it would be resolved against the workspace the model can write.
    pub fn of(entries: &[String]) -> Result<Self, AllowError> {
        let mut parsed = Vec::with_capacity(entries.len());
        for entry in entries {
            if entry.is_empty() {
                return Err(AllowError::Empty);
            }
            if entry.contains('/') {
                let path = Path::new(entry);
                if !path.is_absolute() {
                    return Err(AllowError::Relative(entry.clone()));
                }
                parsed.push(Entry::Absolute(path.to_path_buf()));
            } else {
                parsed.push(Entry::Name(OsString::from(entry)));
            }
        }
        Ok(Self { entries: parsed })
    }

    /// Whether the list permits nothing, which refuses every program.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The entries as the invocation wrote them.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        self.entries.iter().map(ToString::to_string).collect()
    }

    /// Resolves a program name to the one path that will be executed.
    ///
    /// # Errors
    ///
    /// Returns `invalid_args` when the model wrote a path instead of a name,
    /// `program_not_allowed` when no entry permits it, and `not_found` when an
    /// entry names it but it is not on the `PATH`.
    pub fn resolve(
        &self,
        program: &str,
        path: Option<&OsStr>,
    ) -> Result<PathBuf, ToolError> {
        if program.is_empty() || program.contains('/') {
            return Err(ToolError::invalid_args(format!(
                "program is {program:?}; give the name of a program, not a path"
            )));
        }
        let chosen = which(program, path);
        for entry in &self.entries {
            if let (Some(found), Some(candidate)) = (chosen.as_deref(), entry.resolve(path))
                && found == candidate.as_path()
            {
                return Ok(candidate);
            }
        }
        if chosen.is_none() && self.entries.iter().any(|entry| entry.names(program)) {
            return Err(ToolError::not_found(format!(
                "{program} is allowed but is not on the PATH"
            )));
        }
        Err(ToolError::program_not_allowed(self.refusal(program)))
    }

    fn refusal(
        &self,
        program: &str,
    ) -> String {
        let names = self.names().join(", ");
        if names.is_empty() {
            format!("{program} is not allowed; the invocation allows no programs")
        } else {
            format!("{program} is not allowed; the allowed programs are {names}")
        }
    }
}

/// The first executable named `name` on `path`, as `execvp` would find it.
fn which(
    name: &str,
    path: Option<&OsStr>,
) -> Option<PathBuf> {
    let path = path?;
    std::env::split_paths(path)
        .filter(|directory| !directory.as_os_str().is_empty())
        .find_map(|directory| executable(&directory.join(name)))
}

fn executable(path: &Path) -> Option<PathBuf> {
    let canonical = std::fs::canonicalize(path).ok()?;
    let metadata = std::fs::metadata(&canonical).ok()?;
    (metadata.is_file() && metadata.permissions().mode() & 0o111 != 0).then_some(canonical)
}

#[cfg(test)]
mod tests;
