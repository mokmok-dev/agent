//! Host-directory mounts: read-only, read-write, and the shared path
//! resolution that keeps them inside their roots.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::guard::{ByteBudget, PathGuard, check_access};
use super::{DirEntry, Metadata, Vfs};
use crate::vpath::VPath;

/// Resolves a mount-relative virtual path to a host path under `root`,
/// refusing symlink escapes.
///
/// The walk follows existing components so a symlinked directory or file
/// inside the mount cannot redirect a path outside the mount: the deepest
/// existing ancestor is canonicalized and must remain under the (already
/// canonical) mount root.
pub fn resolve_host(
    root: &Path,
    relative: &VPath,
) -> io::Result<PathBuf> {
    let components = relative.components();
    let mut deepest = root.to_path_buf();
    let mut tail: Vec<&str> = Vec::new();
    for (index, component) in components.iter().enumerate() {
        let probe = deepest.join(component);
        match probe.symlink_metadata() {
            Ok(_) => deepest = probe,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                tail = components[index..].iter().map(String::as_str).collect();
                break;
            },
            Err(error) => return Err(error),
        }
    }
    let canonical = deepest.canonicalize()?;
    if !canonical.starts_with(root) {
        return Err(io::Error::from(io::ErrorKind::PermissionDenied));
    }
    let mut host = canonical;
    for component in tail {
        host.push(component);
    }
    Ok(host)
}

fn listing(
    root: &Path,
    guard: &PathGuard,
    path: &VPath,
) -> io::Result<Vec<DirEntry>> {
    let host = resolve_host(root, path)?;
    let mut entries = Vec::new();
    for entry in fs::read_dir(host)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if guard.access_no_ancestors(&path.join(&name)).is_hidden() {
            continue;
        }
        entries.push(DirEntry {
            name,
            is_dir: file_type.is_dir(),
        });
    }
    Ok(entries)
}

fn check_listing_target(
    root: &Path,
    path: &VPath,
) -> io::Result<()> {
    let host = resolve_host(root, path)?;
    match fs::metadata(host) {
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => Err(io::Error::from(io::ErrorKind::NotADirectory)),
        Err(error) => Err(error),
    }
}

/// A host directory exposed read-only: writes through the sandbox are always
/// refused.
pub struct ReadOnlyMount {
    root: PathBuf,
    guard: Arc<PathGuard>,
}

impl ReadOnlyMount {
    /// Creates the mount over an existing, canonical host directory.
    pub const fn new(
        root: PathBuf,
        guard: Arc<PathGuard>,
    ) -> Self {
        Self { root, guard }
    }

    fn check(
        &self,
        path: &VPath,
    ) -> io::Result<()> {
        check_access(&self.guard, path)
    }
}

impl Vfs for ReadOnlyMount {
    fn read(
        &self,
        path: &VPath,
    ) -> io::Result<Vec<u8>> {
        self.check(path)?;
        fs::read(resolve_host(&self.root, path)?)
    }

    fn read_dir(
        &self,
        path: &VPath,
    ) -> io::Result<Vec<DirEntry>> {
        self.check(path)?;
        check_listing_target(&self.root, path)?;
        listing(&self.root, &self.guard, path)
    }

    fn stat(
        &self,
        path: &VPath,
    ) -> io::Result<Metadata> {
        self.check(path)?;
        let host = resolve_host(&self.root, path)?;
        let metadata = fs::metadata(host)?;
        Ok(Metadata {
            is_dir: metadata.is_dir(),
            len: metadata.len(),
        })
    }

    fn write(
        &self,
        path: &VPath,
        _data: &[u8],
    ) -> io::Result<()> {
        self.check(path)?;
        Err(io::Error::from(io::ErrorKind::PermissionDenied))
    }

    fn mkdir(
        &self,
        path: &VPath,
    ) -> io::Result<()> {
        self.check(path)?;
        Err(io::Error::from(io::ErrorKind::PermissionDenied))
    }

    fn remove(
        &self,
        path: &VPath,
    ) -> io::Result<()> {
        self.check(path)?;
        Err(io::Error::from(io::ErrorKind::PermissionDenied))
    }

    fn rename(
        &self,
        from: &VPath,
        to: &VPath,
    ) -> io::Result<()> {
        self.check(from)?;
        self.check(to)?;
        Err(io::Error::from(io::ErrorKind::PermissionDenied))
    }
}

/// A host directory exposed read-write: the only write path to the host.
///
/// The byte budget counts the bytes written through the mount cumulatively;
/// files that already existed on the host before the mount are unknown to it.
pub struct ReadWriteMount {
    root: PathBuf,
    guard: Arc<PathGuard>,
    budget: Arc<ByteBudget>,
}

impl ReadWriteMount {
    /// Creates the mount over an existing, canonical host directory.
    pub const fn new(
        root: PathBuf,
        guard: Arc<PathGuard>,
        budget: Arc<ByteBudget>,
    ) -> Self {
        Self {
            root,
            guard,
            budget,
        }
    }

    fn check(
        &self,
        path: &VPath,
    ) -> io::Result<()> {
        check_access(&self.guard, path)
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
        let parent_host = resolve_host(&self.root, &parent)?;
        if !fs::metadata(&parent_host).is_ok_and(|metadata| metadata.is_dir()) {
            return Err(io::Error::from(io::ErrorKind::NotFound));
        }
        let host = resolve_host(&self.root, path)?;
        self.budget.reserve(data.len() as u64)?;
        match fs::write(&host, data) {
            Ok(()) => Ok(()),
            Err(error) => {
                self.budget.release(data.len() as u64);
                Err(error)
            },
        }
    }
}

impl Vfs for ReadWriteMount {
    fn read(
        &self,
        path: &VPath,
    ) -> io::Result<Vec<u8>> {
        self.check(path)?;
        fs::read(resolve_host(&self.root, path)?)
    }

    fn read_dir(
        &self,
        path: &VPath,
    ) -> io::Result<Vec<DirEntry>> {
        self.check(path)?;
        check_listing_target(&self.root, path)?;
        listing(&self.root, &self.guard, path)
    }

    fn stat(
        &self,
        path: &VPath,
    ) -> io::Result<Metadata> {
        self.check(path)?;
        let host = resolve_host(&self.root, path)?;
        let metadata = fs::metadata(host)?;
        Ok(Metadata {
            is_dir: metadata.is_dir(),
            len: metadata.len(),
        })
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
        fs::create_dir(resolve_host(&self.root, path)?)
    }

    fn remove(
        &self,
        path: &VPath,
    ) -> io::Result<()> {
        self.check(path)?;
        if path.is_root() {
            return Err(io::Error::from(io::ErrorKind::PermissionDenied));
        }
        let host = resolve_host(&self.root, path)?;
        if fs::metadata(&host).is_ok_and(|metadata| metadata.is_file()) {
            fs::remove_file(&host)
        } else {
            fs::remove_dir(&host)
        }
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
        let to_host = resolve_host(&self.root, to)?;
        if fs::metadata(&to_host).is_ok_and(|metadata| metadata.is_dir()) {
            return Err(io::Error::from(io::ErrorKind::IsADirectory));
        }
        let from_host = resolve_host(&self.root, from)?;
        fs::rename(from_host, to_host)
    }
}

#[cfg(test)]
mod tests {
    use super::{ReadOnlyMount, ReadWriteMount, resolve_host};
    use crate::policy::Pattern;
    use crate::vfs::guard::{ByteBudget, PathGuard};
    use crate::vfs::{DirEntry, Vfs};
    use crate::vpath::VPath;
    use std::fs;
    use std::io::ErrorKind;
    use std::os::unix::fs::symlink;
    use std::path::PathBuf;
    use std::sync::Arc;
    use tempfile::TempDir;

    struct Mounts {
        read_only: ReadOnlyMount,
        read_write: ReadWriteMount,
        host: PathBuf,
        _dir: TempDir,
    }

    fn mounts() -> Mounts {
        let dir = TempDir::new().expect("tempdir");
        let host = dir.path().canonicalize().expect("canonical tempdir");
        let read_only = ReadOnlyMount::new(host.join("ro"), plain_guard());
        let read_write = ReadWriteMount::new(
            host.join("rw"),
            plain_guard(),
            Arc::new(ByteBudget::new(Some(16), Some(64))),
        );
        fs::create_dir(&read_only.root).expect("ro dir");
        fs::create_dir(&read_write.root).expect("rw dir");
        Mounts {
            read_only,
            read_write,
            host,
            _dir: dir,
        }
    }

    fn plain_guard() -> Arc<PathGuard> {
        Arc::new(PathGuard::compile(&[], &[]).expect("empty globs"))
    }

    fn filtered_guard() -> Arc<PathGuard> {
        let refuse = vec![Pattern::new(".env")];
        let hide = vec![Pattern::new(".git/**"), Pattern::new(".hidden")];
        Arc::new(PathGuard::compile(&refuse, &hide).expect("valid globs"))
    }

    fn write_host(
        path: &PathBuf,
        data: &[u8],
    ) {
        fs::write(path, data).expect("host write");
    }

    #[test]
    fn resolution_maps_paths_and_clamps_at_the_root() {
        let mounts = mounts();
        let root = mounts.read_write.root.clone();
        let _ = &mounts;
        fs::create_dir_all(root.join("a/b")).expect("dirs");

        assert_eq!(
            resolve_host(&root, &VPath::new("/a/b/c").expect("valid")).expect("resolves"),
            root.join("a/b/c")
        );
        assert_eq!(resolve_host(&root, &VPath::root()).expect("resolves"), root);
    }

    #[test]
    fn symlink_escape_is_refused() {
        let mounts = mounts();
        let root = mounts.read_write.root.clone();
        write_host(&mounts.host.join("secret.txt"), b"outside");
        symlink(mounts.host.join("secret.txt"), root.join("link")).expect("symlink");

        let error = mounts
            .read_write
            .read(&VPath::root().join("link"))
            .expect_err("symlink escape");
        assert_eq!(error.kind(), ErrorKind::PermissionDenied);

        let outside_dir = mounts.host.join("outside");
        fs::create_dir(&outside_dir).expect("outside dir");
        write_host(&outside_dir.join("file"), b"leak");
        symlink(outside_dir, root.join("dir-link")).expect("dir symlink");
        let error = mounts
            .read_write
            .read(&VPath::new("/dir-link/file").expect("valid"))
            .expect_err("symlink escape through a directory");
        assert_eq!(error.kind(), ErrorKind::PermissionDenied);
    }

    #[test]
    fn read_only_mount_refuses_every_mutation() {
        let mounts = mounts();
        write_host(&mounts.read_only.root.join("file.txt"), b"readonly");

        let path = VPath::root().join("file.txt");
        assert_eq!(
            mounts.read_only.read(&path).expect("read"),
            b"readonly".to_vec()
        );
        assert_eq!(
            mounts
                .read_only
                .write(&path, b"x")
                .expect_err("write to read-only")
                .kind(),
            ErrorKind::PermissionDenied
        );
        assert_eq!(
            mounts
                .read_only
                .mkdir(&VPath::root().join("new"))
                .expect_err("mkdir in read-only")
                .kind(),
            ErrorKind::PermissionDenied
        );
        assert_eq!(
            mounts
                .read_only
                .remove(&path)
                .expect_err("remove in read-only")
                .kind(),
            ErrorKind::PermissionDenied
        );
        assert_eq!(
            mounts
                .read_only
                .rename(&path, &VPath::root().join("other"))
                .expect_err("rename in read-only")
                .kind(),
            ErrorKind::PermissionDenied
        );
        assert!(!mounts.read_only.root.join("new").exists());
    }

    #[test]
    fn read_write_mount_round_trips_through_the_host() {
        let mounts = mounts();
        let dir = VPath::root().join("dir");
        let file = dir.join("f.txt");

        mounts.read_write.mkdir(&dir).expect("mkdir");
        assert!(mounts.read_write.root.join("dir").is_dir());
        mounts.read_write.write(&file, b"hello").expect("write");
        assert_eq!(
            mounts.read_write.read(&file).expect("read"),
            b"hello".to_vec()
        );
        assert_eq!(
            mounts.read_write.read_dir(&dir).expect("read_dir"),
            vec![DirEntry {
                name: String::from("f.txt"),
                is_dir: false
            }]
        );

        mounts
            .read_write
            .rename(&file, &dir.join("g.txt"))
            .expect("rename");
        assert!(!mounts.read_write.root.join("dir/f.txt").exists());
        mounts
            .read_write
            .remove(&dir.join("g.txt"))
            .expect("remove");
        assert!(mounts.read_write.read(&dir.join("g.txt")).is_err());
    }

    #[test]
    fn read_write_mount_enforces_globs_and_caps() {
        let dir = TempDir::new().expect("tempdir");
        let mount = ReadWriteMount::new(
            dir.path().canonicalize().expect("canonical tempdir"),
            filtered_guard(),
            Arc::new(ByteBudget::new(Some(4), Some(6))),
        );

        assert_eq!(
            mount
                .read(&VPath::root().join(".env"))
                .expect_err("refused")
                .kind(),
            ErrorKind::PermissionDenied
        );
        assert_eq!(
            mount
                .write(&VPath::root().join("big"), b"12345")
                .expect_err("file cap")
                .kind(),
            ErrorKind::FileTooLarge
        );
        mount
            .write(&VPath::root().join("a"), b"1234")
            .expect("within file cap");
        mount
            .write(&VPath::root().join("b"), b"12")
            .expect("within total cap");
        assert_eq!(
            mount
                .write(&VPath::root().join("c"), b"1")
                .expect_err("total cap")
                .kind(),
            ErrorKind::StorageFull
        );
    }

    #[test]
    fn listings_filter_hidden_entries() {
        let dir = TempDir::new().expect("tempdir");
        let mount = ReadWriteMount::new(
            dir.path().canonicalize().expect("canonical tempdir"),
            filtered_guard(),
            Arc::new(ByteBudget::new(None, None)),
        );
        write_host(&dir.path().join("visible.txt"), b"v");
        write_host(&dir.path().join(".hidden"), b"h");
        fs::create_dir(dir.path().join(".git")).expect("dir");
        write_host(&dir.path().join(".git/config"), b"c");

        let entries = mount.read_dir(&VPath::root()).expect("read_dir");
        let names: Vec<&str> = entries.iter().map(DirEntry::name).collect();
        assert!(names.contains(&"visible.txt"));
        assert!(
            names.contains(&".git"),
            "the .git directory itself is not hidden"
        );
        assert!(!names.contains(&".hidden"));
    }
}
