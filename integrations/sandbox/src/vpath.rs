//! Sandbox-absolute virtual paths.

use std::fmt::{self, Display, Formatter};

/// A normalized, sandbox-absolute virtual path such as `/work/src/lib.rs`.
///
/// Normalization removes duplicate and trailing separators and empty
/// components (`.`), and clamps `..` at the root, so no [`Vfs`](crate::vfs::Vfs)
/// implementation ever sees an escape attempt inside a path value.
/// The root is `/`.
#[derive(Clone, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct VPath {
    components: Vec<String>,
}

impl VPath {
    /// Parses and normalizes a sandbox-absolute path.
    ///
    /// Returns `None` for paths that are not absolute (they do not start with
    /// `/`) or that contain no usable path at all beyond their separators.
    #[must_use]
    pub fn new(path: &str) -> Option<Self> {
        let stripped = path.strip_prefix('/')?;

        let mut components = Vec::new();
        for component in stripped.split('/') {
            match component {
                "" | "." => {},
                ".." => {
                    // Clamped at the root: `..` beyond the top stays at `/`.
                    components.pop();
                },
                _ => components.push(String::from(component)),
            }
        }

        Some(Self { components })
    }

    /// The root path `/`.
    #[must_use]
    pub const fn root() -> Self {
        Self {
            components: Vec::new(),
        }
    }

    /// Whether this is the root path `/`.
    #[must_use]
    pub const fn is_root(&self) -> bool {
        self.components.is_empty()
    }

    /// The path components without separators.
    #[must_use]
    pub fn components(&self) -> &[String] {
        &self.components
    }

    /// The last component, or `""` at the root.
    #[must_use]
    pub fn name(&self) -> &str {
        self.components.last().map_or("", String::as_str)
    }

    /// The parent path, or `None` at the root.
    #[must_use]
    pub fn parent(&self) -> Option<Self> {
        let mut components = self.components.clone();
        components.pop()?;
        Some(Self { components })
    }

    /// The parent path, with the root as its own parent.
    #[must_use]
    pub fn parent_or_root(&self) -> Self {
        self.parent().unwrap_or_else(Self::root)
    }

    /// Joins a single component, producing a child path.
    ///
    /// # Panics
    ///
    /// Panics if `name` contains a separator, is empty, or is `.` or `..`;
    /// children are always built from directory-entry names.
    #[must_use]
    pub fn join(
        &self,
        name: &str,
    ) -> Self {
        assert!(
            !name.is_empty() && !name.contains('/') && name != "." && name != "..",
            "a joined name must be a single path component: {name:?}"
        );
        let mut components = self.components.clone();
        components.push(String::from(name));
        Self { components }
    }

    /// Whether `prefix` is a prefix of this path.
    #[must_use]
    pub fn starts_with(
        &self,
        prefix: &Self,
    ) -> bool {
        self.components.get(..prefix.components.len()) == Some(prefix.components.as_slice())
    }

    /// The path relative to `prefix`, or `None` when `prefix` is not a prefix.
    #[must_use]
    pub fn strip_prefix(
        &self,
        prefix: &Self,
    ) -> Option<Self> {
        if !self.starts_with(prefix) {
            return None;
        }
        let rest = self.components.get(prefix.components.len()..)?;
        Some(Self {
            components: rest.to_vec(),
        })
    }

    /// The immediate parent chain, nearest parent first, excluding the root.
    pub fn ancestors(&self) -> impl Iterator<Item = Self> {
        let components = self.components.clone();
        (0..components.len())
            .rev()
            .map(move |len| Self {
                components: components[..len].to_vec(),
            })
            .take_while(|path| !path.is_root())
    }
}

impl Display for VPath {
    fn fmt(
        &self,
        formatter: &mut Formatter<'_>,
    ) -> fmt::Result {
        formatter.write_str("/")?;
        for (index, component) in self.components.iter().enumerate() {
            if index > 0 {
                formatter.write_str("/")?;
            }
            formatter.write_str(component)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::VPath;

    fn path(value: &str) -> VPath {
        VPath::new(value).expect("a valid sandbox-absolute path")
    }

    #[test]
    fn normalizes_separators_and_dot_components() {
        assert_eq!(path("/a//b/./c/").to_string(), "/a/b/c");
        assert_eq!(VPath::root().to_string(), "/");
        assert_eq!(path("/").to_string(), "/");
        assert_eq!(path("///").to_string(), "/");
    }

    #[test]
    fn clamps_parent_traversal_at_the_root() {
        assert_eq!(path("/a/../b").to_string(), "/b");
        assert_eq!(path("/../../a").to_string(), "/a");
        assert_eq!(path("/a/b/../..").to_string(), "/");
    }

    #[test]
    fn rejects_relative_paths() {
        assert!(VPath::new("a/b").is_none());
        assert!(VPath::new("").is_none());
        assert!(VPath::new("./a").is_none());
    }

    #[test]
    fn navigation_helpers() {
        let target = path("/a/b/c.txt");

        assert_eq!(target.name(), "c.txt");
        assert_eq!(
            target.parent().as_ref().map(VPath::to_string),
            Some("/a/b".to_string())
        );
        assert!(!target.is_root());
        assert!(VPath::root().is_root());
        assert_eq!(VPath::root().name(), "");
        assert!(VPath::root().parent().is_none());

        assert_eq!(
            target
                .ancestors()
                .map(|ancestor| ancestor.to_string())
                .collect::<Vec<_>>(),
            ["/a/b", "/a"]
        );
        assert!(path("/a").ancestors().next().is_none());
    }

    #[test]
    fn prefix_operations() {
        let prefix = path("/a");
        let inside = path("/a/b/c");
        let outside = path("/ab/c");

        assert!(inside.starts_with(&prefix));
        assert!(!outside.starts_with(&prefix));
        assert_eq!(
            inside.strip_prefix(&prefix).map(|rest| rest.to_string()),
            Some("/b/c".to_string())
        );
        assert!(outside.strip_prefix(&prefix).is_none());
        assert_eq!(
            inside.strip_prefix(&inside).map(|rest| rest.to_string()),
            Some("/".to_string())
        );
    }

    #[test]
    #[should_panic(expected = "single path component")]
    fn join_rejects_separators() {
        let _ = path("/a").join("b/c");
    }
}
