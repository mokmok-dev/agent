//! Shared path screening (refuse/hide globs) and byte accounting.

use std::io;
use std::sync::atomic::{AtomicU64, Ordering};

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};

use crate::error::SandboxError;
use crate::policy::Pattern;
use crate::vpath::VPath;

/// The outcome of screening a path against the refuse and hide globs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Access {
    /// The path is neither refused nor hidden.
    Allowed,
    /// A refuse glob matched: access is denied.
    Refused,
    /// A hide glob matched: the path appears absent.
    Hidden,
}

impl Access {
    /// The I/O error a non-allowed path produces: refused is a permission
    /// failure, hidden is absence.
    pub fn to_error(self) -> io::Error {
        match self {
            Self::Allowed | Self::Hidden => io::Error::from(io::ErrorKind::NotFound),
            Self::Refused => io::Error::from(io::ErrorKind::PermissionDenied),
        }
    }

    /// Whether the path should be skipped in directory listings.
    pub fn is_hidden(self) -> bool {
        self == Self::Hidden
    }
}

/// Checks a screened path, turning the outcome into an I/O result.
pub fn check_access(
    guard: &PathGuard,
    path: &VPath,
) -> io::Result<()> {
    match guard.access(path) {
        Access::Allowed => Ok(()),
        access => Err(access.to_error()),
    }
}

/// The compiled refuse and hide globs of a [`Policy`](crate::policy::Policy).
pub struct PathGuard {
    refuse: GlobSet,
    hide: GlobSet,
}

impl PathGuard {
    /// Compiles the globs.
    ///
    /// # Errors
    ///
    /// Invalid patterns fail closed with [`SandboxError::InvalidPolicy`].
    pub fn compile(
        refuse: &[Pattern],
        hide: &[Pattern],
    ) -> Result<Self, SandboxError> {
        Ok(Self {
            refuse: compile_globset(refuse)?,
            hide: compile_globset(hide)?,
        })
    }

    /// Screens a path, including its ancestors: deny wins over hide, and
    /// hiding (or refusing) an ancestor hides everything beneath it.
    pub fn access(
        &self,
        path: &VPath,
    ) -> Access {
        let mut chain = vec![path.clone()];
        chain.extend(path.ancestors());

        for screened in &chain {
            if self.screen(screened) == Some(Access::Refused) {
                return Access::Refused;
            }
        }
        for screened in &chain {
            if self.screen(screened) == Some(Access::Hidden) {
                return Access::Hidden;
            }
        }
        Access::Allowed
    }

    /// Screens only the path itself, without its ancestors.
    pub fn access_no_ancestors(
        &self,
        path: &VPath,
    ) -> Access {
        self.screen(path).unwrap_or(Access::Allowed)
    }

    fn screen(
        &self,
        path: &VPath,
    ) -> Option<Access> {
        for candidate in Self::candidates(path) {
            if self.refuse.is_match(&candidate) {
                return Some(Access::Refused);
            }
        }
        for candidate in Self::candidates(path) {
            if self.hide.is_match(&candidate) {
                return Some(Access::Hidden);
            }
        }
        None
    }

    /// The strings a glob may match for a path: the path itself and every
    /// suffix starting after a separator, giving patterns deny-anywhere
    /// semantics (`.env` denies `/work/.env` and `/work/sub/.env`).
    fn candidates(path: &VPath) -> impl Iterator<Item = String> {
        let components = path.components();
        let full = path.to_string();
        let suffixes = (0..components.len()).map(move |skip| components[skip..].join("/"));
        std::iter::once(full).chain(suffixes)
    }
}

/// Enforces the [`FsPolicy`](crate::policy::FsPolicy) byte caps. The budget is
/// shared by every mount of one sandbox so `max_total_bytes` counts all
/// writes through the sandbox.
pub struct ByteBudget {
    max_file_bytes: Option<u64>,
    max_total_bytes: Option<u64>,
    used: AtomicU64,
}

impl ByteBudget {
    /// Creates a budget from the policy caps.
    pub const fn new(
        max_file_bytes: Option<u64>,
        max_total_bytes: Option<u64>,
    ) -> Self {
        Self {
            max_file_bytes,
            max_total_bytes,
            used: AtomicU64::new(0),
        }
    }

    /// Reserves `len` bytes, enforcing both caps. Callers must
    /// [`ByteBudget::release`] if the subsequent write fails.
    pub fn reserve(
        &self,
        len: u64,
    ) -> io::Result<()> {
        if let Some(max_file_bytes) = self.max_file_bytes
            && len > max_file_bytes
        {
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                format!("write of {len} bytes exceeds the {max_file_bytes} byte file cap"),
            ));
        }
        if let Some(max_total_bytes) = self.max_total_bytes {
            loop {
                let used = self.used.load(Ordering::Relaxed);
                if used.saturating_add(len) > max_total_bytes {
                    return Err(io::Error::new(
                        io::ErrorKind::StorageFull,
                        format!(
                            "write of {len} bytes exceeds the {max_total_bytes} byte total cap"
                        ),
                    ));
                }
                if self
                    .used
                    .compare_exchange(used, used + len, Ordering::Relaxed, Ordering::Relaxed)
                    .is_ok()
                {
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    /// Returns reserved bytes to the budget, e.g. after a failed write.
    pub fn release(
        &self,
        len: u64,
    ) {
        self.used.fetch_sub(len, Ordering::Relaxed);
    }
}

fn compile_globset(patterns: &[Pattern]) -> Result<GlobSet, SandboxError> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob = GlobBuilder::new(pattern.as_str())
            .build()
            .map_err(|error| {
                SandboxError::InvalidPolicy(format!("invalid glob {:?}: {error}", pattern.as_str()))
            })?;
        builder.add(glob);
    }
    builder
        .build()
        .map_err(|error| SandboxError::InvalidPolicy(format!("globs failed to compile: {error}")))
}

#[cfg(test)]
mod tests {
    use super::{Access, ByteBudget, PathGuard};
    use crate::policy::Pattern;
    use crate::vpath::VPath;
    use std::io::ErrorKind;

    fn guard(
        refuse: &[&str],
        hide: &[&str],
    ) -> PathGuard {
        let refuse: Vec<Pattern> = refuse
            .iter()
            .map(|pattern| Pattern::new(*pattern))
            .collect();
        let hide: Vec<Pattern> = hide.iter().map(|pattern| Pattern::new(*pattern)).collect();
        PathGuard::compile(&refuse, &hide).expect("valid globs")
    }

    #[test]
    fn patterns_match_anywhere_in_the_tree() {
        let guard = guard(&[".env", "*.pem"], &[".git/**"]);

        assert_eq!(
            guard.access(&VPath::new("/work/.env").expect("valid")),
            Access::Refused
        );
        assert_eq!(
            guard.access(&VPath::new("/work/sub/.env").expect("valid")),
            Access::Refused
        );
        assert_eq!(
            guard.access(&VPath::new("/work/key.pem").expect("valid")),
            Access::Refused
        );
        assert_eq!(
            guard.access(&VPath::new("/work/.git/objects/ab").expect("valid")),
            Access::Hidden
        );
        assert_eq!(
            guard.access(&VPath::new("/work/src/main.rs").expect("valid")),
            Access::Allowed
        );
    }

    #[test]
    fn deny_wins_over_hide_along_the_whole_chain() {
        let guard = guard(&[".env"], &["**"]);

        // The whole tree is hidden, but a refused path stays refused.
        assert_eq!(
            guard.access(&VPath::new("/a/.env").expect("valid")),
            Access::Refused
        );
        assert_eq!(
            guard.access(&VPath::new("/a/b").expect("valid")),
            Access::Hidden
        );

        // A hidden ancestor hides descendants even when they match nothing.
        assert_eq!(
            guard.access(&VPath::new("/hidden/child").expect("valid")),
            Access::Hidden
        );
    }

    #[test]
    fn anchors_match_the_mount_root() {
        let guard = guard(&["/.env"], &[]);

        assert_eq!(
            guard.access(&VPath::new("/.env").expect("valid")),
            Access::Refused
        );
        assert_eq!(
            guard.access(&VPath::new("/sub/.env").expect("valid")),
            Access::Allowed
        );
    }

    #[test]
    fn invalid_patterns_fail_closed() {
        let patterns = vec![Pattern::new("[unclosed")];
        let error = PathGuard::compile(&patterns, &[])
            .err()
            .expect("an invalid glob is rejected");

        assert!(matches!(
            error,
            crate::error::SandboxError::InvalidPolicy(_)
        ));
    }

    #[test]
    fn byte_budget_enforces_both_caps() {
        let budget = ByteBudget::new(Some(10), Some(20));

        budget.reserve(8).expect("within the file cap");
        budget.reserve(8).expect("within the total cap");
        let file_cap_error = budget.reserve(11).expect_err("over the file cap");
        assert_eq!(file_cap_error.kind(), ErrorKind::FileTooLarge);
        let total_cap_error = budget.reserve(8).expect_err("over the total cap");
        assert_eq!(total_cap_error.kind(), ErrorKind::StorageFull);

        budget.release(8);
        budget.reserve(10).expect("fits after releasing");
    }
}
