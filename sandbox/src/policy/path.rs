//! Lexical path checks shared by the policy domains.
//!
//! These are *lexical* checks: they constrain the shape of a path without
//! touching the filesystem, so the policy core can validate without a host.
//! Resolving symlinks, collapsing `/tmp` to `/private/tmp`, and rejecting an
//! out-of-root symlink all need the filesystem, so they belong to the renderer
//! and executor milestones; see `docs/sandbox/filesystem.md`.

use std::path::{Component, Path};

use super::InvalidPolicy;

/// Check that `path` is absolute and has no `..` component.
///
/// A `..` component would let an entry escape the root it appears to name, so a
/// path that is not already resolved is rejected rather than silently
/// reinterpreted. A `.` component is collapsed by [`Path::components`] and so is
/// allowed: it names the same location and cannot escape.
///
/// # Errors
///
/// Returns [`InvalidPolicy::NotAbsolute`] for a relative path and
/// [`InvalidPolicy::NotNormalized`] for one with a `..` component.
pub(super) fn check_absolute_normalized(path: &Path) -> Result<(), InvalidPolicy> {
    if !path.is_absolute() {
        return Err(InvalidPolicy::NotAbsolute {
            path: path.to_path_buf(),
        });
    }
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(InvalidPolicy::NotNormalized {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

/// Whether `ancestor` is `descendant` or an ancestor of it.
///
/// This is the "covers" relation used by validation: a `deny` that covers a
/// write root or the working directory would nullify it. The comparison is
/// component-wise, so `/a/bc` is not covered by `/a/b`.
pub(super) fn covers(
    ancestor: &Path,
    descendant: &Path,
) -> bool {
    descendant.starts_with(ancestor)
}

#[cfg(test)]
mod tests {
    // Tests for the lexical path checks: absolute, no `..`, and component-wise
    // coverage.

    use super::*;

    #[test]
    fn a_relative_path_is_rejected() {
        assert!(matches!(
            check_absolute_normalized(Path::new("a/b")),
            Err(InvalidPolicy::NotAbsolute { .. })
        ));
    }

    #[test]
    fn a_parent_component_is_rejected() {
        assert!(matches!(
            check_absolute_normalized(Path::new("/a/../b")),
            Err(InvalidPolicy::NotNormalized { .. })
        ));
    }

    #[test]
    fn an_absolute_normalized_path_is_accepted() {
        assert!(check_absolute_normalized(Path::new("/a/b/c")).is_ok());
        assert!(check_absolute_normalized(Path::new("/")).is_ok());
    }

    #[test]
    fn a_root_covers_everything() {
        assert!(covers(Path::new("/"), Path::new("/a/b")));
    }

    #[test]
    fn a_path_covers_itself() {
        assert!(covers(Path::new("/a/b"), Path::new("/a/b")));
    }

    #[test]
    fn coverage_is_component_wise_not_byte_wise() {
        assert!(!covers(Path::new("/a/b"), Path::new("/a/bc")));
        assert!(covers(Path::new("/a/b"), Path::new("/a/b/c")));
        assert!(!covers(Path::new("/a/b"), Path::new("/a")));
    }
}
