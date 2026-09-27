//! The filesystem domain: path entries and protected metadata names.

use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{Access, InvalidPolicy, path};

/// The names protected read-only inside every write root unless overridden.
///
/// `.git` is the history a command is diffed against and `.agents` holds the
/// instructions it follows, so a confined command must not rewrite either. See
/// `docs/sandbox/filesystem.md`.
pub const DEFAULT_PROTECTED: [&str; 2] = [".git", ".agents"];

/// The filesystem domain of a [`Policy`](super::Policy).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FsPolicy {
    /// Path entries, evaluated with `deny > write > read`.
    #[serde(default)]
    pub entries: Vec<FsEntry>,
    /// Names forced read-only inside any write root.
    ///
    /// Defaults to [`DEFAULT_PROTECTED`]. Emptying it removes the protection,
    /// which is a deliberate operator choice rather than a silent default.
    #[serde(default = "default_protected")]
    pub protected: Vec<String>,
}

impl Default for FsPolicy {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            protected: default_protected(),
        }
    }
}

/// The protected names a default [`FsPolicy`] carries.
fn default_protected() -> Vec<String> {
    DEFAULT_PROTECTED
        .iter()
        .map(|name| (*name).to_owned())
        .collect()
}

/// One path entry: a location and the access it grants or removes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FsEntry {
    /// An absolute, resolved path naming a directory subtree or a literal file.
    pub path: PathBuf,
    /// The access this entry grants or removes.
    pub access: Access,
}

impl FsEntry {
    /// A read-only entry rooted at `path`.
    #[must_use]
    pub fn read(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            access: Access::Read,
        }
    }

    /// A read-write entry rooted at `path`.
    #[must_use]
    pub fn write(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            access: Access::Write,
        }
    }

    /// A deny entry that removes access to `path` and everything under it.
    #[must_use]
    pub fn deny(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            access: Access::Deny,
        }
    }
}

impl FsPolicy {
    /// Validate the entries and protected names against each other.
    ///
    /// This checks shape, not the host: every path must be absolute and free of
    /// `..`, every protected name must be a single normal component, and no
    /// `deny` may cover a write root.
    ///
    /// Two `deny` checks the design lists are deferred to the executor
    /// milestone, because they need the host: a `deny` over the **executable
    /// directory** (the shell is resolved from `PATH` at spawn) and over the
    /// **scratch directory** (the executor creates it). The working directory is
    /// known here, so [`Policy`](super::Policy) checks it.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidPolicy`] for the first violation found.
    pub fn validate(&self) -> Result<(), InvalidPolicy> {
        for entry in &self.entries {
            path::check_absolute_normalized(&entry.path)?;
        }
        for name in &self.protected {
            validate_protected_name(name)?;
        }
        self.reject_denied_write_roots()
    }

    /// The effective access for `target`, with `deny > write > read`, and with
    /// every protected name capped at read.
    ///
    /// A target matched by no entry has the base **read** grant: reads are
    /// broad, the whole host root, and a `deny` narrows them. So the zero policy
    /// grants no write and no network but still lets a command read.
    ///
    /// When several entries match, the most restrictive wins, so a `deny` nested
    /// inside a broader `write` root holds. [`Access`]'s `Ord` is the precedence
    /// order, so `max` is "most restrictive".
    ///
    /// A `write` is then capped at [`Access::Read`] when the target lies inside
    /// a write root's protected name, so `.git` cannot be rewritten or freshly
    /// created even though its parent is writable.
    #[must_use]
    pub fn access_for(
        &self,
        target: &Path,
    ) -> Access {
        let base = self
            .entries
            .iter()
            .filter(|entry| path::covers(&entry.path, target))
            .map(|entry| entry.access)
            .max()
            .unwrap_or(Access::Read);
        if base == Access::Write && self.is_protected(target) {
            return Access::Read;
        }
        base
    }

    /// Whether `target` lies inside a write root's protected name.
    ///
    /// The protected path is `<write-root>/<name>`, matching the rendering in
    /// `docs/sandbox/filesystem.md`, and everything under it is covered. A
    /// protected name nested deeper than one level below a write root (for
    /// example `/work/project/.git`) is **not** covered by this rule; that
    /// matches the Seatbelt profile's `^<root>/<name>(/.*)?$` carve-out.
    fn is_protected(
        &self,
        target: &Path,
    ) -> bool {
        self.entries
            .iter()
            .filter(|entry| entry.access == Access::Write)
            .any(|root| {
                self.protected
                    .iter()
                    .any(|name| path::covers(&root.path.join(name.as_str()), target))
            })
    }

    /// Reject a `deny` that covers a `write` root.
    ///
    /// A deny over a write root would make the root unconditionally unwritable,
    /// which is never the intent of a `write` entry; failing closed beats
    /// silently producing a policy that cannot work.
    fn reject_denied_write_roots(&self) -> Result<(), InvalidPolicy> {
        for deny in self.entries.iter().filter(|e| e.access == Access::Deny) {
            for root in self.entries.iter().filter(|e| e.access == Access::Write) {
                if path::covers(&deny.path, &root.path) {
                    return Err(InvalidPolicy::DenyCoversWriteRoot {
                        deny: deny.path.clone(),
                        root: root.path.clone(),
                    });
                }
            }
        }
        Ok(())
    }
}

/// Check that a protected name is a single, normal path component.
fn validate_protected_name(name: &str) -> Result<(), InvalidPolicy> {
    let as_path = Path::new(name);
    let is_single_normal = !name.is_empty()
        && as_path.components().count() == 1
        && matches!(as_path.components().next(), Some(Component::Normal(_)));
    if is_single_normal {
        Ok(())
    } else {
        Err(InvalidPolicy::InvalidProtectedName {
            name: name.to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    // Tests for the filesystem domain: path entry precedence, the deny-over-
    // write rule, protected-name validation and enforcement, and the lexical
    // path checks.

    use super::*;

    /// A write root used by several cases.
    fn write_root() -> PathBuf {
        PathBuf::from("/work")
    }

    #[test]
    fn an_unmatched_path_is_readable_by_default() {
        // Reads are the broad host grant a `deny` narrows, so the zero policy
        // still lets a command read; it grants no write.
        let fs = FsPolicy::default();
        assert_eq!(fs.access_for(Path::new("/etc/hosts")), Access::Read);
    }

    #[test]
    fn a_deny_removes_the_default_read() {
        let fs = FsPolicy {
            entries: vec![FsEntry::deny("/etc/secrets")],
            protected: Vec::new(),
        };
        assert_eq!(fs.access_for(Path::new("/etc/secrets/key")), Access::Deny);
        assert_eq!(fs.access_for(Path::new("/etc/hosts")), Access::Read);
    }

    #[test]
    fn a_deny_inside_a_write_root_holds() {
        let fs = FsPolicy {
            entries: vec![FsEntry::write("/work"), FsEntry::deny("/work/secret")],
            protected: Vec::new(),
        };
        assert_eq!(fs.access_for(Path::new("/work/secret/key")), Access::Deny);
        assert_eq!(fs.access_for(Path::new("/work/public")), Access::Write);
    }

    #[test]
    fn the_most_restrictive_matching_entry_wins() {
        let fs = FsPolicy {
            entries: vec![
                FsEntry::read("/work"),
                FsEntry::write("/work/out"),
                FsEntry::deny("/work/out/keep"),
            ],
            protected: Vec::new(),
        };
        assert_eq!(fs.access_for(Path::new("/work/a")), Access::Read);
        // The broader `read` also matches, but `write` is more restrictive.
        assert_eq!(fs.access_for(Path::new("/work/out/a")), Access::Write);
        // The deeper `deny` beats both.
        assert_eq!(fs.access_for(Path::new("/work/out/keep/a")), Access::Deny);
    }

    #[test]
    fn a_sibling_with_a_shared_prefix_is_not_covered() {
        // `/work` must not cover `/workspace`: coverage is component-wise.
        let fs = FsPolicy {
            entries: vec![FsEntry::write("/work")],
            protected: Vec::new(),
        };
        assert_eq!(fs.access_for(Path::new("/workspace/file")), Access::Read);
        assert_eq!(fs.access_for(Path::new("/work")), Access::Write);
    }

    #[test]
    fn the_default_protected_names_are_git_and_agents() {
        assert_eq!(FsPolicy::default().protected, vec![".git", ".agents"]);
    }

    #[test]
    fn a_protected_name_inside_a_write_root_is_read_only() {
        let fs = FsPolicy {
            entries: vec![FsEntry::write("/work")],
            protected: vec![".git".to_owned()],
        };
        // The directory itself and everything under it are capped at read.
        assert_eq!(fs.access_for(Path::new("/work/.git")), Access::Read);
        assert_eq!(fs.access_for(Path::new("/work/.git/config")), Access::Read);
        // Its siblings are unaffected.
        assert_eq!(fs.access_for(Path::new("/work/src")), Access::Write);
    }

    #[test]
    fn a_protected_name_is_capped_in_every_write_root() {
        // The cap is a property of the name, not of one root: with two write
        // roots, both protect `.git`.
        let fs = FsPolicy {
            entries: vec![FsEntry::write("/work"), FsEntry::write("/data")],
            protected: vec![".git".to_owned()],
        };
        assert_eq!(fs.access_for(Path::new("/work/.git/x")), Access::Read);
        assert_eq!(fs.access_for(Path::new("/data/.git/x")), Access::Read);
    }

    #[test]
    fn an_empty_protected_list_removes_the_cap() {
        let fs = FsPolicy {
            entries: vec![FsEntry::write("/work")],
            protected: Vec::new(),
        };
        assert_eq!(fs.access_for(Path::new("/work/.git/config")), Access::Write);
    }

    #[test]
    fn the_protected_cap_beats_an_explicit_write_entry() {
        // Protection is forced, not a default an entry can override; emptying
        // `protected` is the deliberate way to lift it.
        let fs = FsPolicy {
            entries: vec![FsEntry::write("/work"), FsEntry::write("/work/.git")],
            protected: vec![".git".to_owned()],
        };
        assert_eq!(fs.access_for(Path::new("/work/.git/HEAD")), Access::Read);
    }

    #[test]
    fn a_protected_name_deeper_than_one_level_is_not_capped() {
        // Only `<write-root>/<name>` is protected, matching the renderer's
        // carve-out; a nested repository's `.git` is not covered by it.
        let fs = FsPolicy {
            entries: vec![FsEntry::write("/work")],
            protected: vec![".git".to_owned()],
        };
        assert_eq!(
            fs.access_for(Path::new("/work/project/.git/config")),
            Access::Write
        );
    }

    #[test]
    fn a_protected_name_outside_every_write_root_is_moot() {
        let fs = FsPolicy {
            entries: vec![FsEntry::write("/work")],
            protected: vec![".git".to_owned()],
        };
        // `/etc/.git` is read already, so the cap changes nothing.
        assert_eq!(fs.access_for(Path::new("/etc/.git/config")), Access::Read);
    }

    #[test]
    fn a_relative_entry_is_rejected() {
        let fs = FsPolicy {
            entries: vec![FsEntry::read("relative/path")],
            protected: Vec::new(),
        };
        assert!(matches!(
            fs.validate(),
            Err(InvalidPolicy::NotAbsolute { .. })
        ));
    }

    #[test]
    fn an_entry_with_a_parent_component_is_rejected() {
        let fs = FsPolicy {
            entries: vec![FsEntry::read("/work/../etc")],
            protected: Vec::new(),
        };
        assert!(matches!(
            fs.validate(),
            Err(InvalidPolicy::NotNormalized { .. })
        ));
    }

    #[test]
    fn a_dot_component_is_allowed_because_it_cannot_escape() {
        let fs = FsPolicy {
            entries: vec![FsEntry::read("/work/./sub")],
            protected: Vec::new(),
        };
        assert!(fs.validate().is_ok());
    }

    #[test]
    fn a_protected_name_that_is_a_path_is_rejected() {
        for name in ["", "/abs", "a/b", ".", ".."] {
            let fs = FsPolicy {
                entries: Vec::new(),
                protected: vec![name.to_owned()],
            };
            assert!(
                matches!(
                    fs.validate(),
                    Err(InvalidPolicy::InvalidProtectedName { .. })
                ),
                "`{name}` should be rejected"
            );
        }
    }

    #[test]
    fn a_deny_over_a_write_root_is_rejected_at_construction() {
        let fs = FsPolicy {
            entries: vec![FsEntry::write("/work"), FsEntry::deny("/work")],
            protected: Vec::new(),
        };
        assert!(matches!(
            fs.validate(),
            Err(InvalidPolicy::DenyCoversWriteRoot { .. })
        ));
    }

    #[test]
    fn a_deny_over_a_nested_write_root_is_rejected() {
        let fs = FsPolicy {
            entries: vec![FsEntry::write("/work/out"), FsEntry::deny("/work")],
            protected: Vec::new(),
        };
        assert!(matches!(
            fs.validate(),
            Err(InvalidPolicy::DenyCoversWriteRoot { .. })
        ));
    }

    #[test]
    fn a_deny_of_a_sibling_of_a_write_root_is_allowed() {
        let fs = FsPolicy {
            entries: vec![FsEntry::write("/work"), FsEntry::deny("/secrets")],
            protected: Vec::new(),
        };
        assert!(fs.validate().is_ok());
    }

    #[test]
    fn the_write_root_covered_by_an_entry_is_itself_writable() {
        let fs = FsPolicy {
            entries: vec![FsEntry::write("/work")],
            protected: Vec::new(),
        };
        assert_eq!(fs.access_for(&write_root()), Access::Write);
    }
}
