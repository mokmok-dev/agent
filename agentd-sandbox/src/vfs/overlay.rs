//! The copy-on-write overlay backend: reads fall through to the host layer,
//! writes stay in memory.

use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use super::guard::{ByteBudget, PathGuard, check_access};
use super::mount::resolve_host;
use super::tree::{MemTree, Node, Written};
use super::{DirEntry, Metadata, Vfs};
use crate::vpath::VPath;

struct UpperState {
    tree: MemTree,
    tombstones: BTreeSet<VPath>,
}

type UpperGuard<'a> = MutexGuard<'a, UpperState>;

/// What the overlay presents for a path: the upper layer first, then the
/// host layer, minus tombstones.
enum Logical {
    /// A file written through the upper layer, shadowing any host file.
    UpperFile(Vec<u8>),
    /// A directory written through the upper layer.
    UpperDir,
    /// A file that exists only on the host layer.
    HostFile {
        /// The host file's length.
        len: u64,
    },
    /// A directory that exists only on the host layer.
    HostDir,
    /// Neither layer has the path.
    Absent,
}

/// An overlay over an existing host directory.
///
/// Reads fall through to the host layer; every write lands in the in-memory
/// upper layer, so the host side is never written through the sandbox. A
/// removed host entry becomes a tombstone in the upper layer so it disappears
/// from reads and listings without touching the host.
pub struct Overlay {
    root: PathBuf,
    guard: Arc<PathGuard>,
    budget: Arc<ByteBudget>,
    state: Mutex<UpperState>,
}

impl Overlay {
    /// Creates the overlay over an existing, canonical host directory.
    pub const fn new(
        root: PathBuf,
        guard: Arc<PathGuard>,
        budget: Arc<ByteBudget>,
    ) -> Self {
        Self {
            root,
            guard,
            budget,
            state: Mutex::new(UpperState {
                tree: MemTree::new(),
                tombstones: BTreeSet::new(),
            }),
        }
    }

    fn lock(&self) -> io::Result<UpperGuard<'_>> {
        self.state
            .lock()
            .map_err(|poisoned: PoisonError<UpperGuard<'_>>| io::Error::other(poisoned.to_string()))
    }

    fn check(
        &self,
        path: &VPath,
    ) -> io::Result<()> {
        check_access(&self.guard, path)
    }

    fn logical(
        &self,
        state: &UpperState,
        path: &VPath,
    ) -> io::Result<Logical> {
        if state.tombstones.contains(path) {
            return Ok(Logical::Absent);
        }
        match state.tree.get(path) {
            Some(Node::File(data)) => return Ok(Logical::UpperFile(data.clone())),
            Some(Node::Dir) => return Ok(Logical::UpperDir),
            None => {},
        }
        if path.is_root() {
            return Ok(Logical::UpperDir);
        }
        let host = resolve_host(&self.root, path)?;
        match fs::metadata(host) {
            Ok(metadata) if metadata.is_dir() => Ok(Logical::HostDir),
            Ok(metadata) => Ok(Logical::HostFile {
                len: metadata.len(),
            }),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Logical::Absent),
            Err(error) => Err(error),
        }
    }

    fn child_access(
        &self,
        dir: &VPath,
        name: &str,
    ) -> bool {
        self.guard.access_no_ancestors(&dir.join(name)).is_hidden()
    }

    fn host_has_children(
        &self,
        path: &VPath,
    ) -> io::Result<bool> {
        Ok(fs::read_dir(resolve_host(&self.root, path)?)?
            .next()
            .is_some())
    }

    /// Writes into the upper layer, charging the byte budget.
    fn write_upper(
        &self,
        state: &mut UpperState,
        path: &VPath,
        data: &[u8],
    ) -> io::Result<()> {
        let len = data.len() as u64;
        self.budget.reserve(len)?;
        match state.tree.replace(path, data.to_vec()) {
            Written::Replaced(previous_len) => self.budget.release(previous_len),
            Written::New => {},
        }
        state.tombstones.remove(path);
        Ok(())
    }

    /// Whether the parent of a path exists as a logical directory in either
    /// layer.
    fn parent_is_dir(
        &self,
        state: &UpperState,
        parent: &VPath,
    ) -> io::Result<bool> {
        Ok(state.tree.is_dir(parent)
            || (!state.tombstones.contains(parent) && self.host_is_dir(parent)?))
    }

    fn host_is_dir(
        &self,
        path: &VPath,
    ) -> io::Result<bool> {
        Ok(fs::metadata(resolve_host(&self.root, path)?).is_ok_and(|metadata| metadata.is_dir()))
    }

    fn checked_write(
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
            let mut state = self.lock()?;
            match self.logical(&state, path)? {
                Logical::UpperDir | Logical::HostDir => {
                    return Err(io::Error::from(io::ErrorKind::IsADirectory));
                },
                Logical::UpperFile(_) | Logical::HostFile { .. } | Logical::Absent => {},
            }
            if !self.parent_is_dir(&state, &parent)? {
                return Err(io::Error::from(io::ErrorKind::NotFound));
            }
            self.write_upper(&mut state, path, data)?;
        }
        Ok(())
    }
}

impl Vfs for Overlay {
    fn read(
        &self,
        path: &VPath,
    ) -> io::Result<Vec<u8>> {
        self.check(path)?;
        let logical = {
            let state = self.lock()?;
            self.logical(&state, path)?
        };
        match logical {
            Logical::UpperFile(data) => Ok(data),
            Logical::HostFile { .. } => fs::read(resolve_host(&self.root, path)?),
            Logical::UpperDir | Logical::HostDir => {
                Err(io::Error::from(io::ErrorKind::IsADirectory))
            },
            Logical::Absent => Err(io::Error::from(io::ErrorKind::NotFound)),
        }
    }

    fn read_dir(
        &self,
        path: &VPath,
    ) -> io::Result<Vec<DirEntry>> {
        self.check(path)?;
        let entries = {
            let state = self.lock()?;
            match self.logical(&state, path)? {
                Logical::UpperDir | Logical::HostDir => {},
                Logical::UpperFile(_) | Logical::HostFile { .. } => {
                    return Err(io::Error::from(io::ErrorKind::NotADirectory));
                },
                Logical::Absent => return Err(io::Error::from(io::ErrorKind::NotFound)),
            }

            let mut seen: HashSet<String> = HashSet::new();
            let mut entries = Vec::new();

            for (name, node) in state.tree.children(path) {
                if seen.insert(name.clone()) && !self.child_access(path, &name) {
                    entries.push(DirEntry {
                        name,
                        is_dir: matches!(node, Node::Dir),
                    });
                }
            }
            for entry in fs::read_dir(resolve_host(&self.root, path)?)? {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().into_owned();
                if state.tombstones.contains(&path.join(&name)) {
                    continue;
                }
                if seen.insert(name.clone()) && !self.child_access(path, &name) {
                    let is_dir = entry.file_type()?.is_dir();
                    entries.push(DirEntry { name, is_dir });
                }
            }
            entries
        };
        Ok(entries)
    }

    fn stat(
        &self,
        path: &VPath,
    ) -> io::Result<Metadata> {
        self.check(path)?;
        let metadata = {
            let state = self.lock()?;
            match self.logical(&state, path)? {
                Logical::UpperFile(data) => Metadata {
                    is_dir: false,
                    len: data.len() as u64,
                },
                Logical::HostFile { len } => Metadata { is_dir: false, len },
                Logical::UpperDir | Logical::HostDir => Metadata {
                    is_dir: true,
                    len: 0,
                },
                Logical::Absent => {
                    return Err(io::Error::from(io::ErrorKind::NotFound));
                },
            }
        };
        Ok(metadata)
    }

    fn write(
        &self,
        path: &VPath,
        data: &[u8],
    ) -> io::Result<()> {
        self.checked_write(path, data)
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
            let mut state = self.lock()?;
            if !matches!(self.logical(&state, path)?, Logical::Absent) {
                return Err(io::Error::from(io::ErrorKind::AlreadyExists));
            }
            if !self.parent_is_dir(&state, &parent)? {
                return Err(io::Error::from(io::ErrorKind::NotFound));
            }
            state.tree.insert(path, Node::Dir);
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
            let mut state = self.lock()?;
            match self.logical(&state, path)? {
                Logical::UpperFile(_) => {
                    if let Some(Node::File(data)) = state.tree.remove(path) {
                        self.budget.release(data.len() as u64);
                    }
                },
                Logical::UpperDir => {
                    if state.tree.is_empty_dir(path) {
                        state.tree.remove(path);
                    } else {
                        return Err(io::Error::from(io::ErrorKind::DirectoryNotEmpty));
                    }
                },
                Logical::HostDir => {
                    if state.tree.is_empty_dir(path) && !self.host_has_children(path)? {
                        state.tombstones.insert(path.clone());
                    } else {
                        return Err(io::Error::from(io::ErrorKind::DirectoryNotEmpty));
                    }
                },
                Logical::HostFile { .. } => {
                    state.tombstones.insert(path.clone());
                },
                Logical::Absent => return Err(io::Error::from(io::ErrorKind::NotFound)),
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
            let state = self.lock()?;
            match (self.logical(&state, from)?, self.logical(&state, to)?) {
                (Logical::UpperDir | Logical::HostDir, _) => {
                    return Err(io::Error::from(io::ErrorKind::InvalidInput));
                },
                (Logical::Absent, _) => {
                    return Err(io::Error::from(io::ErrorKind::NotFound));
                },
                (_, Logical::UpperDir | Logical::HostDir) => {
                    return Err(io::Error::from(io::ErrorKind::IsADirectory));
                },
                _ => {},
            }
        }
        if from == to {
            return Ok(());
        }
        let data = self.read(from)?;

        {
            // The target write is charged (and can fail on the byte caps), so
            // it must succeed before the source is removed or tombstoned.
            let mut state = self.lock()?;
            if !self.parent_is_dir(&state, &to.parent_or_root())? {
                return Err(io::Error::from(io::ErrorKind::NotFound));
            }
            self.write_upper(&mut state, to, &data)?;
            if let Some(Node::File(previous)) = state.tree.remove(from) {
                self.budget.release(previous.len() as u64);
            } else {
                state.tombstones.insert(from.clone());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::Overlay;
    use crate::policy::Pattern;
    use crate::vfs::guard::{ByteBudget, PathGuard};
    use crate::vfs::{DirEntry, Vfs};
    use crate::vpath::VPath;
    use std::fs;
    use std::io::ErrorKind;
    use std::path::PathBuf;
    use std::sync::Arc;
    use tempfile::TempDir;

    struct Fixture {
        overlay: Overlay,
        host: PathBuf,
        _dir: TempDir,
    }

    fn overlay() -> Fixture {
        let dir = TempDir::new().expect("tempdir");
        let host = dir.path().canonicalize().expect("canonical tempdir");
        fs::create_dir(host.join("lower-dir")).expect("lower dir");
        fs::write(host.join("lower-dir/lower-file"), b"l").expect("lower file");
        fs::write(host.join("lower.txt"), b"lower").expect("lower file");
        Fixture {
            overlay: Overlay::new(
                host.clone(),
                Arc::new(PathGuard::compile(&[], &[]).expect("empty globs")),
                Arc::new(ByteBudget::new(Some(8), None)),
            ),
            host,
            _dir: dir,
        }
    }

    fn refused_overlay() -> (Overlay, TempDir) {
        let dir = TempDir::new().expect("tempdir");
        let host = dir.path().canonicalize().expect("canonical tempdir");
        fs::write(host.join(".env"), b"secret").expect("refused file");
        fs::create_dir(host.join(".git")).expect("hidden dir");
        let refuse = vec![Pattern::new(".env")];
        let hide = vec![Pattern::new(".git"), Pattern::new(".git/**")];
        let overlay = Overlay::new(
            host,
            Arc::new(PathGuard::compile(&refuse, &hide).expect("valid globs")),
            Arc::new(ByteBudget::new(None, None)),
        );
        (overlay, dir)
    }

    #[test]
    fn reads_fall_through_to_the_host() {
        let fixture = overlay();

        assert_eq!(
            fixture
                .overlay
                .read(&VPath::root().join("lower.txt"))
                .expect("read lower file"),
            b"lower".to_vec()
        );
        assert!(
            fixture
                .overlay
                .stat(&VPath::root().join("lower-dir"))
                .expect("stat lower dir")
                .is_dir
        );
    }

    #[test]
    fn writes_stay_in_memory_and_never_touch_the_host() {
        let fixture = overlay();
        let upper_file = VPath::root().join("upper.txt");

        fixture.overlay.write(&upper_file, b"upper").expect("write");
        assert_eq!(
            fixture.overlay.read(&upper_file).expect("read"),
            b"upper".to_vec()
        );
        assert!(!fixture.host.join("upper.txt").exists());

        let shadowed = VPath::root().join("lower.txt");
        fixture
            .overlay
            .write(&shadowed, b"redo")
            .expect("shadow write");
        assert_eq!(
            fixture.overlay.read(&shadowed).expect("read"),
            b"redo".to_vec()
        );
        assert_eq!(
            fs::read(fixture.host.join("lower.txt")).expect("host untouched"),
            b"lower".to_vec()
        );
    }

    #[test]
    fn writes_require_a_logical_parent_directory() {
        let fixture = overlay();

        assert_eq!(
            fixture
                .overlay
                .write(&VPath::new("/absent/f").expect("valid"), b"x")
                .expect_err("no parent")
                .kind(),
            ErrorKind::NotFound
        );
        fixture
            .overlay
            .write(&VPath::new("/lower-dir/f").expect("valid"), b"x")
            .expect("host parent directory");
    }

    #[test]
    fn listings_merge_both_layers() {
        let fixture = overlay();
        let dir = VPath::root().join("lower-dir");
        fixture
            .overlay
            .write(&dir.join("upper-child"), b"u")
            .expect("write");

        let entries = fixture
            .overlay
            .read_dir(&VPath::root())
            .expect("root listing");
        let names: Vec<&str> = entries.iter().map(DirEntry::name).collect();
        assert!(names.contains(&"lower.txt"));
        assert!(names.contains(&"lower-dir"));

        let merged = fixture.overlay.read_dir(&dir).expect("merged listing");
        assert!(merged.iter().any(|entry| entry.name() == "upper-child"));
    }

    #[test]
    fn tombstones_remove_host_files_without_touching_them() {
        let fixture = overlay();
        let lower = VPath::root().join("lower.txt");

        fixture.overlay.remove(&lower).expect("remove lower file");
        assert_eq!(
            fixture.overlay.stat(&lower).expect_err("tombstoned").kind(),
            ErrorKind::NotFound
        );
        assert!(fixture.host.join("lower.txt").exists());

        let entries = fixture.overlay.read_dir(&VPath::root()).expect("listing");
        assert!(!entries.iter().any(|entry| entry.name == "lower.txt"));
    }

    #[test]
    fn remove_enforces_empty_directories_across_layers() {
        let fixture = overlay();
        let dir = VPath::root().join("lower-dir");
        let file = dir.join("upper-child");
        fixture.overlay.write(&file, b"u").expect("write");

        assert_eq!(
            fixture.overlay.remove(&dir).expect_err("not empty").kind(),
            ErrorKind::DirectoryNotEmpty
        );
        fixture.overlay.remove(&file).expect("remove upper file");
        assert_eq!(
            fixture
                .overlay
                .remove(&dir)
                .expect_err("lower dir not empty")
                .kind(),
            ErrorKind::DirectoryNotEmpty
        );
    }

    #[test]
    fn rename_copies_lower_files_into_the_upper_layer() {
        let fixture = overlay();
        let from = VPath::root().join("lower.txt");
        let to = VPath::root().join("moved.txt");

        fixture.overlay.rename(&from, &to).expect("rename");
        assert_eq!(fixture.overlay.read(&to).expect("read"), b"lower".to_vec());
        assert_eq!(
            fixture
                .overlay
                .stat(&from)
                .expect_err("tombstoned source")
                .kind(),
            ErrorKind::NotFound
        );
        assert!(fixture.host.join("lower.txt").exists());
    }

    #[test]
    fn failed_rename_leaves_the_source_intact() {
        let dir = TempDir::new().expect("tempdir");
        let host = dir.path().canonicalize().expect("canonical tempdir");
        fs::write(host.join("source.txt"), b"payload").expect("lower file");
        let refuse: Vec<Pattern> = Vec::new();
        let overlay = Overlay::new(
            host,
            Arc::new(PathGuard::compile(&refuse, &refuse).expect("empty globs")),
            Arc::new(ByteBudget::new(Some(4), Some(4))),
        );

        // The 8 byte file exceeds the caps: the rename must fail and leave
        // the source exactly where it was.
        let error = overlay
            .rename(
                &VPath::root().join("source.txt"),
                &VPath::root().join("moved.txt"),
            )
            .expect_err("over the cap");
        assert_eq!(error.kind(), ErrorKind::FileTooLarge);
        assert_eq!(
            overlay
                .read(&VPath::root().join("source.txt"))
                .expect("source intact"),
            b"payload".to_vec()
        );

        // Renaming into a directory that does not exist fails cleanly.
        let missing_parent = overlay
            .rename(
                &VPath::root().join("source.txt"),
                &VPath::new("/absent/moved.txt").expect("valid"),
            )
            .expect_err("no parent");
        assert_eq!(missing_parent.kind(), ErrorKind::NotFound);
        assert_eq!(
            overlay
                .read(&VPath::root().join("source.txt"))
                .expect("source intact"),
            b"payload".to_vec()
        );
    }

    #[test]
    fn byte_caps_apply_to_the_upper_layer_only() {
        let fixture = overlay();

        assert_eq!(
            fixture
                .overlay
                .write(&VPath::root().join("big"), &[0; 9])
                .expect_err("over the file cap")
                .kind(),
            ErrorKind::FileTooLarge
        );
        fixture
            .overlay
            .write(&VPath::root().join("ok"), &[0; 8])
            .expect("within the file cap");
    }

    #[test]
    fn globs_apply_across_layers() {
        let (overlay, _dir) = refused_overlay();

        assert_eq!(
            overlay
                .stat(&VPath::root().join(".env"))
                .expect_err("refused")
                .kind(),
            ErrorKind::PermissionDenied
        );
        assert_eq!(
            overlay
                .read(&VPath::new("/.git/config").expect("valid"))
                .expect_err("hidden")
                .kind(),
            ErrorKind::NotFound
        );
        let entries = overlay.read_dir(&VPath::root()).expect("listing");
        let names: Vec<&str> = entries.iter().map(DirEntry::name).collect();
        // Refused entries are still listed (access to them is denied), while
        // hidden entries are absent from listings.
        assert!(names.contains(&".env"));
        assert!(!names.contains(&".git"));
    }
}
