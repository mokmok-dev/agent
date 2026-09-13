//! The fully in-memory VFS backend.

use std::io;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use super::guard::{Access, ByteBudget, PathGuard, check_access};
use super::tree::{MemTree, Node, Written};
use super::{DirEntry, Metadata, Vfs};
use crate::vpath::VPath;

type TreeGuard<'a> = MutexGuard<'a, MemTree>;

/// A byte-accounted, fully in-memory filesystem: the default and the safest
/// mount. Nothing touches the host.
pub struct Mem {
    guard: Arc<PathGuard>,
    budget: Arc<ByteBudget>,
    tree: Mutex<MemTree>,
}

impl Mem {
    /// Creates the backend with the shared policy guard and byte budget.
    pub const fn new(
        guard: Arc<PathGuard>,
        budget: Arc<ByteBudget>,
    ) -> Self {
        Self {
            guard,
            budget,
            tree: Mutex::new(MemTree::new()),
        }
    }

    fn lock(&self) -> io::Result<TreeGuard<'_>> {
        self.tree
            .lock()
            .map_err(|poisoned: PoisonError<TreeGuard<'_>>| io::Error::other(poisoned.to_string()))
    }

    /// Screens the path and its ancestors against the policy.
    fn check(
        &self,
        path: &VPath,
    ) -> io::Result<()> {
        check_access(&self.guard, path)
    }

    fn child_access(
        &self,
        dir: &VPath,
        name: &str,
    ) -> Access {
        self.guard.access_no_ancestors(&dir.join(name))
    }
}

impl Vfs for Mem {
    fn read(
        &self,
        path: &VPath,
    ) -> io::Result<Vec<u8>> {
        self.check(path)?;
        let tree = self.lock()?;
        match tree.get(path) {
            Some(Node::File(data)) => Ok(data.clone()),
            Some(Node::Dir) => Err(io::Error::from(io::ErrorKind::IsADirectory)),
            None => Err(io::Error::from(io::ErrorKind::NotFound)),
        }
    }

    fn read_dir(
        &self,
        path: &VPath,
    ) -> io::Result<Vec<DirEntry>> {
        self.check(path)?;
        let tree = self.lock()?;
        if tree.is_dir(path) {
            Ok(tree
                .children(path)
                .into_iter()
                .filter(|(name, _)| !self.child_access(path, name).is_hidden())
                .map(|(name, node)| DirEntry {
                    name,
                    is_dir: matches!(node, Node::Dir),
                })
                .collect())
        } else if tree.get(path).is_some() {
            Err(io::Error::from(io::ErrorKind::NotADirectory))
        } else {
            Err(io::Error::from(io::ErrorKind::NotFound))
        }
    }

    fn stat(
        &self,
        path: &VPath,
    ) -> io::Result<Metadata> {
        self.check(path)?;
        let tree = self.lock()?;
        match tree.get(path) {
            Some(Node::File(data)) => Ok(Metadata {
                is_dir: false,
                len: data.len() as u64,
            }),
            Some(Node::Dir) => Ok(Metadata {
                is_dir: true,
                len: 0,
            }),
            None => Err(io::Error::from(io::ErrorKind::NotFound)),
        }
    }

    fn write(
        &self,
        path: &VPath,
        data: &[u8],
    ) -> io::Result<()> {
        self.check(path)?;
        if path.is_root() {
            return Err(io::Error::from(io::ErrorKind::IsADirectory));
        }
        let parent = path.parent_or_root();
        {
            let mut tree = self.lock()?;
            if !tree.is_dir(&parent) {
                return Err(io::Error::from(io::ErrorKind::NotFound));
            }
            if tree.is_dir(path) {
                return Err(io::Error::from(io::ErrorKind::IsADirectory));
            }
            self.budget.reserve(data.len() as u64)?;
            match tree.replace(path, data.to_vec()) {
                Written::Replaced(previous_len) => self.budget.release(previous_len),
                Written::New => {},
            }
        }
        Ok(())
    }

    fn mkdir(
        &self,
        path: &VPath,
    ) -> io::Result<()> {
        self.check(path)?;
        if path.is_root() {
            return Err(io::Error::from(io::ErrorKind::AlreadyExists));
        }
        let parent = path.parent_or_root();
        {
            let mut tree = self.lock()?;
            if tree.get(path).is_some() {
                return Err(io::Error::from(io::ErrorKind::AlreadyExists));
            }
            if !tree.is_dir(&parent) {
                return Err(io::Error::from(io::ErrorKind::NotFound));
            }
            tree.insert(path, Node::Dir);
        }
        Ok(())
    }

    fn remove(
        &self,
        path: &VPath,
    ) -> io::Result<()> {
        self.check(path)?;
        if path.is_root() {
            return Err(io::Error::from(io::ErrorKind::PermissionDenied));
        }
        {
            let mut tree = self.lock()?;
            if tree.is_dir(path) {
                if !tree.is_empty_dir(path) {
                    return Err(io::Error::from(io::ErrorKind::DirectoryNotEmpty));
                }
                tree.remove(path);
            } else if let Some(Node::File(data)) = tree.remove(path) {
                self.budget.release(data.len() as u64);
            } else {
                return Err(io::Error::from(io::ErrorKind::NotFound));
            }
        }
        Ok(())
    }

    fn rename(
        &self,
        from: &VPath,
        to: &VPath,
    ) -> io::Result<()> {
        self.check(from)?;
        self.check(to)?;
        if from.is_root() || to.is_root() {
            return Err(io::Error::from(io::ErrorKind::InvalidInput));
        }
        {
            let mut tree = self.lock()?;
            let data = match tree.get(from) {
                Some(Node::File(data)) => data.clone(),
                Some(Node::Dir) => return Err(io::Error::from(io::ErrorKind::InvalidInput)),
                None => return Err(io::Error::from(io::ErrorKind::NotFound)),
            };
            if !tree.is_dir(&to.parent_or_root()) {
                return Err(io::Error::from(io::ErrorKind::NotFound));
            }
            if tree.is_dir(to) {
                return Err(io::Error::from(io::ErrorKind::IsADirectory));
            }
            tree.remove(from);
            if let Some(previous_len) = tree.file_len(to) {
                self.budget.release(previous_len);
            }
            tree.insert(to, Node::File(data));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{Mem, Node};
    use crate::error::SandboxError;
    use crate::policy::Pattern;
    use crate::vfs::guard::{ByteBudget, PathGuard};
    use crate::vfs::{DirEntry, Metadata, Vfs};
    use crate::vpath::VPath;
    use std::io;
    use std::io::ErrorKind;
    use std::sync::Arc;

    fn setup() -> Mem {
        Mem::new(
            Arc::new(PathGuard::compile(&[], &[]).expect("empty globs compile")),
            Arc::new(ByteBudget::new(None, None)),
        )
    }

    fn denied_mem() -> Mem {
        let refuse = vec![Pattern::new(".env"), Pattern::new("secret/**")];
        let hide = vec![Pattern::new(".git/**"), Pattern::new(".hidden")];
        Mem::new(
            Arc::new(PathGuard::compile(&refuse, &hide).expect("valid globs")),
            Arc::new(ByteBudget::new(Some(8), Some(12))),
        )
    }

    fn kind(error: &io::Error) -> ErrorKind {
        error.kind()
    }

    #[test]
    fn read_write_stat_round_trip() {
        let mem = setup();
        let dir = VPath::root().join("dir");
        let file = dir.join("f.txt");

        mem.mkdir(&dir).expect("mkdir");
        mem.write(&file, b"hello").expect("write");
        assert_eq!(mem.read(&file).expect("read"), b"hello".to_vec());
        assert_eq!(
            mem.stat(&file).expect("stat"),
            Metadata {
                is_dir: false,
                len: 5
            }
        );
        assert!(mem.stat(&dir).expect("stat").is_dir);
        assert_eq!(
            mem.read_dir(&dir).expect("read_dir"),
            vec![DirEntry {
                name: String::from("f.txt"),
                is_dir: false
            }]
        );
    }

    #[test]
    fn strict_creation_requires_existing_parents() {
        let mem = setup();

        assert_eq!(
            kind(
                &mem.write(&VPath::new("/a/b").expect("valid"), b"x")
                    .expect_err("no parent")
            ),
            ErrorKind::NotFound
        );
        assert_eq!(
            kind(
                &mem.mkdir(&VPath::new("/a/b").expect("valid"))
                    .expect_err("no parent")
            ),
            ErrorKind::NotFound
        );
        mem.mkdir(&VPath::root().join("a"))
            .expect("mkdir root child");
        assert_eq!(
            kind(
                &mem.mkdir(&VPath::root().join("a"))
                    .expect_err("already exists")
            ),
            ErrorKind::AlreadyExists
        );
    }

    #[test]
    fn write_never_targets_a_directory() {
        let mem = setup();
        let dir = VPath::root().join("dir");
        mem.mkdir(&dir).expect("mkdir");

        assert_eq!(
            kind(&mem.write(&dir, b"x").expect_err("directory target")),
            ErrorKind::IsADirectory
        );
        assert_eq!(
            kind(&mem.read(&dir).expect_err("directory read")),
            ErrorKind::IsADirectory
        );
        assert_eq!(
            kind(&mem.read(&VPath::root().join("absent")).expect_err("absent")),
            ErrorKind::NotFound
        );
    }

    #[test]
    fn remove_enforces_empty_directories_and_frees_bytes() {
        let mem = setup();
        let dir = VPath::root().join("dir");
        let file = dir.join("f");
        mem.mkdir(&dir).expect("mkdir");
        mem.write(&file, b"data").expect("write");

        assert_eq!(
            kind(&mem.remove(&dir).expect_err("not empty")),
            ErrorKind::DirectoryNotEmpty
        );
        mem.remove(&file).expect("remove file");
        mem.remove(&dir).expect("remove empty dir");
        assert!(mem.stat(&dir).is_err());
    }

    #[test]
    fn rename_moves_files_only() {
        let mem = setup();
        let from = VPath::root().join("from.txt");
        let to = VPath::root().join("to.txt");
        mem.write(&from, b"payload").expect("write");

        mem.rename(&from, &to).expect("rename");
        assert!(mem.stat(&from).is_err());
        assert_eq!(mem.read(&to).expect("read"), b"payload".to_vec());

        let dir = VPath::root().join("dir");
        mem.mkdir(&dir).expect("mkdir");
        assert_eq!(
            kind(&mem.rename(&dir, &to).expect_err("directory rename")),
            ErrorKind::InvalidInput
        );
    }

    #[test]
    fn refused_paths_are_permission_denied_and_hidden_are_absent() {
        let mem = denied_mem();

        assert_eq!(
            kind(&mem.read(&VPath::root().join(".env")).expect_err("refused")),
            ErrorKind::PermissionDenied
        );
        assert_eq!(
            kind(
                &mem.write(&VPath::new("/secret/key").expect("valid"), b"x")
                    .expect_err("refused")
            ),
            ErrorKind::PermissionDenied
        );
        assert_eq!(
            kind(
                &mem.stat(&VPath::new("/.git/config").expect("valid"))
                    .expect_err("hidden")
            ),
            ErrorKind::NotFound
        );
    }

    #[test]
    fn listings_skip_hidden_entries() {
        let mem = denied_mem();
        let dir = VPath::root().join("d");
        mem.mkdir(&dir).expect("mkdir");
        mem.write(&dir.join("visible"), b"v").expect("write");
        // A hidden path can never be created through the VFS, so seed the
        // tree directly to observe the listing filter.
        mem.tree
            .lock()
            .expect("tree lock")
            .insert(&dir.join(".hidden"), Node::File(Vec::new()));

        assert_eq!(
            mem.read_dir(&dir).expect("read_dir"),
            vec![DirEntry {
                name: String::from("visible"),
                is_dir: false
            }]
        );
    }

    #[test]
    fn byte_caps_abort_oversized_writes() {
        let mem = denied_mem();
        let file = VPath::root().join("big");

        assert_eq!(
            kind(&mem.write(&file, &[0; 9]).expect_err("over the file cap")),
            ErrorKind::FileTooLarge
        );
        mem.write(&file, &[0; 8]).expect("at the file cap");
        assert_eq!(
            kind(
                &mem.write(&VPath::root().join("other"), b"12345")
                    .expect_err("over total cap")
            ),
            ErrorKind::StorageFull
        );
    }

    #[test]
    fn ancestors_hidden_by_patterns_block_children() {
        let hide = vec![Pattern::new("hidden/**")];
        let mem = Mem::new(
            Arc::new(PathGuard::compile(&[], &hide).expect("valid globs")),
            Arc::new(ByteBudget::new(None, None)),
        );
        let hidden_file = VPath::new("/hidden/child/f").expect("valid");

        assert_eq!(
            kind(&mem.stat(&hidden_file).expect_err("hidden")),
            ErrorKind::NotFound
        );
    }

    #[test]
    fn construction_rejects_invalid_globs_at_compile_time() {
        let patterns = vec![Pattern::new("[unclosed")];
        assert!(matches!(
            PathGuard::compile(&patterns, &[]),
            Err(SandboxError::InvalidPolicy(_))
        ));
    }
}
