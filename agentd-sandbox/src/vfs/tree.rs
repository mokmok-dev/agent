//! The in-memory node tree backing `Mem` and the overlay upper layer.

use std::collections::BTreeMap;

use crate::vpath::VPath;

pub enum Node {
    Dir,
    File(Vec<u8>),
}

pub enum Written {
    /// The written path did not hold a file before.
    New,
    /// The written path replaced a file of the given length.
    Replaced(u64),
}

/// A tree of files and directories rooted at the mount point. The root is
/// always a directory and never stored.
pub struct MemTree {
    nodes: BTreeMap<VPath, Node>,
}

impl MemTree {
    pub const fn new() -> Self {
        Self {
            nodes: BTreeMap::new(),
        }
    }

    pub fn get(
        &self,
        path: &VPath,
    ) -> Option<&Node> {
        if path.is_root() {
            return Some(&Node::Dir);
        }
        self.nodes.get(path)
    }

    pub fn is_dir(
        &self,
        path: &VPath,
    ) -> bool {
        matches!(self.get(path), Some(Node::Dir))
    }

    pub fn file_len(
        &self,
        path: &VPath,
    ) -> Option<u64> {
        match self.get(path) {
            Some(Node::File(data)) => Some(data.len() as u64),
            _ => None,
        }
    }

    pub fn insert(
        &mut self,
        path: &VPath,
        node: Node,
    ) {
        if !path.is_root() {
            self.nodes.insert(path.clone(), node);
        }
    }

    pub fn remove(
        &mut self,
        path: &VPath,
    ) -> Option<Node> {
        self.nodes.remove(path)
    }

    pub fn replace(
        &mut self,
        path: &VPath,
        data: Vec<u8>,
    ) -> Written {
        match self.nodes.insert(path.clone(), Node::File(data)) {
            Some(Node::File(previous)) => Written::Replaced(previous.len() as u64),
            Some(Node::Dir) | None => Written::New,
        }
    }

    /// The direct children of a directory as `(name, node)` pairs.
    pub fn children(
        &self,
        dir: &VPath,
    ) -> Vec<(String, &Node)> {
        self.nodes
            .iter()
            .filter(|(path, _)| path.parent().as_ref() == Some(dir))
            .map(|(path, node)| (String::from(path.name()), node))
            .collect()
    }

    /// Whether a directory has no direct children.
    pub fn is_empty_dir(
        &self,
        dir: &VPath,
    ) -> bool {
        self.children(dir).is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::{MemTree, Node};
    use crate::vpath::VPath;

    fn root() -> VPath {
        VPath::root()
    }

    fn child(
        parent: &VPath,
        name: &str,
    ) -> VPath {
        parent.join(name)
    }

    #[test]
    fn root_is_an_implicit_directory() {
        let tree = MemTree::new();

        assert!(tree.is_dir(&root()));
        assert!(tree.get(&root()).is_some());
        assert!(tree.children(&root()).is_empty());
    }

    #[test]
    fn children_lists_only_direct_children() {
        let mut tree = MemTree::new();
        tree.insert(&root().join("a"), Node::Dir);
        tree.insert(&child(&root().join("a"), "b"), Node::Dir);
        tree.insert(&child(&root().join("a"), "c"), Node::File(vec![1]));
        tree.insert(&root().join("ab"), Node::Dir);

        let names: Vec<String> = tree
            .children(&root().join("a"))
            .into_iter()
            .map(|(name, _)| name)
            .collect();

        assert_eq!(names, ["b", "c"]);
    }

    #[test]
    fn replace_reports_what_it_displaced() {
        let mut tree = MemTree::new();
        let path = root().join("f");

        tree.insert(&path, Node::File(vec![1, 2, 3]));
        match tree.replace(&path, vec![4]) {
            super::Written::Replaced(previous_len) => assert_eq!(previous_len, 3),
            super::Written::New => panic!("expected a replacement"),
        }
        match tree.replace(&path, vec![5]) {
            super::Written::Replaced(previous_len) => assert_eq!(previous_len, 1),
            super::Written::New => panic!("expected a replacement"),
        }
    }
}
