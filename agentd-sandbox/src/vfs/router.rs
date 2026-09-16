//! The mount router: the single virtual filesystem plane built from a policy.

use std::io;
use std::path::Path;
use std::sync::Arc;

use super::guard::{ByteBudget, PathGuard};
use super::mem::Mem;
use super::mount::{ReadOnlyMount, ReadWriteMount};
use super::overlay::Overlay;
use super::{DirEntry, Metadata, Vfs};
use crate::error::SandboxError;
use crate::policy::{FsPolicy, MountSource};
use crate::vpath::VPath;

enum Backend {
    Mem(Mem),
    ReadOnly(ReadOnlyMount),
    ReadWrite(ReadWriteMount),
    Overlay(Overlay),
}

struct Mounted {
    at: VPath,
    backend: Backend,
}

/// The virtual filesystem every execution path shares, assembled from the
/// [`FsPolicy`](crate::policy::FsPolicy) mounts.
///
/// Path lookups route to the longest-prefix mount; paths no mount covers are
/// absent. The refuse and hide globs and the byte caps are shared by every
/// mount, so a `.pem` refused in one mount is refused through every consumer
/// of the sandbox.
pub struct MountedVfs {
    mounts: Vec<Mounted>,
}

impl std::fmt::Debug for MountedVfs {
    fn fmt(
        &self,
        formatter: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        let points: Vec<String> = self
            .mounts
            .iter()
            .map(|mounted| mounted.at.to_string())
            .collect();
        formatter
            .debug_struct("MountedVfs")
            .field("mounts", &points)
            .finish()
    }
}

impl MountedVfs {
    /// Builds the virtual filesystem from the policy.
    ///
    /// # Errors
    ///
    /// Fails closed with [`SandboxError::InvalidPolicy`] for invalid mount
    /// points, missing or non-directory host roots, duplicate mount points,
    /// and invalid glob patterns.
    pub fn from_policy(fs: &FsPolicy) -> Result<Self, SandboxError> {
        let guard = Arc::new(PathGuard::compile(&fs.refuse, &fs.hide)?);
        let budget = Arc::new(ByteBudget::new(fs.max_file_bytes, fs.max_total_bytes));

        let mut mounts = Vec::with_capacity(fs.mounts.len());
        for mount in &fs.mounts {
            let at = virtual_point(&mount.at)?;
            let backend = build_backend(&mount.source, &guard, &budget)?;
            mounts.push(Mounted { at, backend });
        }
        for (index, first) in mounts.iter().enumerate() {
            for second in &mounts[(index + 1)..] {
                if first.at == second.at {
                    return Err(SandboxError::InvalidPolicy(format!(
                        "duplicate mount point {}",
                        first.at
                    )));
                }
            }
        }
        mounts.sort_by(|first, second| {
            second
                .at
                .components()
                .len()
                .cmp(&first.at.components().len())
        });
        Ok(Self { mounts })
    }

    /// Resolves a path to its longest-prefix mount and the path relative to
    /// that mount.
    fn route(
        &self,
        path: &VPath,
    ) -> io::Result<(&Backend, VPath)> {
        for mounted in &self.mounts {
            if let Some(stripped) = path.strip_prefix(&mounted.at) {
                return Ok((&mounted.backend, stripped));
            }
        }
        Err(io::Error::from(io::ErrorKind::NotFound))
    }

    /// Whether two routed paths live in the same backend mount.
    fn same_backend(
        first: &Backend,
        second: &Backend,
    ) -> bool {
        match (first, second) {
            (Backend::Mem(a), Backend::Mem(b)) => std::ptr::eq(a, b),
            (Backend::ReadOnly(a), Backend::ReadOnly(b)) => std::ptr::eq(a, b),
            (Backend::ReadWrite(a), Backend::ReadWrite(b)) => std::ptr::eq(a, b),
            (Backend::Overlay(a), Backend::Overlay(b)) => std::ptr::eq(a, b),
            _ => false,
        }
    }
}

impl Vfs for MountedVfs {
    fn read(
        &self,
        path: &VPath,
    ) -> io::Result<Vec<u8>> {
        let (backend, stripped) = self.route(path)?;
        backend.read(&stripped)
    }

    fn read_dir(
        &self,
        path: &VPath,
    ) -> io::Result<Vec<DirEntry>> {
        let (backend, stripped) = self.route(path)?;
        let mut entries = backend.read_dir(&stripped)?;

        // Sibling mount points appear as directories in the listing of the
        // path that contains them.
        if stripped.is_root() {
            for mounted in &self.mounts {
                if mounted.at.parent().as_ref() != Some(path) {
                    continue;
                }
                let name = mounted.at.name();
                if entries.iter().any(|entry| entry.name() == name) {
                    continue;
                }
                entries.push(DirEntry {
                    name: String::from(name),
                    is_dir: true,
                });
            }
        }
        Ok(entries)
    }

    fn stat(
        &self,
        path: &VPath,
    ) -> io::Result<Metadata> {
        let (backend, stripped) = self.route(path)?;
        backend.stat(&stripped)
    }

    fn write(
        &self,
        path: &VPath,
        data: &[u8],
    ) -> io::Result<()> {
        let (backend, stripped) = self.route(path)?;
        backend.write(&stripped, data)
    }

    fn mkdir(
        &self,
        path: &VPath,
    ) -> io::Result<()> {
        let (backend, stripped) = self.route(path)?;
        backend.mkdir(&stripped)
    }

    fn remove(
        &self,
        path: &VPath,
    ) -> io::Result<()> {
        let (backend, stripped) = self.route(path)?;
        backend.remove(&stripped)
    }

    fn rename(
        &self,
        from: &VPath,
        to: &VPath,
    ) -> io::Result<()> {
        let (from_backend, from_stripped) = self.route(from)?;
        let (to_backend, to_stripped) = self.route(to)?;

        if Self::same_backend(from_backend, to_backend) {
            return from_backend.rename(&from_stripped, &to_stripped);
        }
        // A cross-mount move is a copy through the trait plus a source
        // removal; directories are not supported.
        if self.stat(from)?.is_dir {
            return Err(io::Error::from(io::ErrorKind::InvalidInput));
        }
        let data = self.read(from)?;
        self.write(to, &data)?;
        self.remove(from)
    }
}

impl Vfs for Backend {
    fn read(
        &self,
        path: &VPath,
    ) -> io::Result<Vec<u8>> {
        match self {
            Self::Mem(backend) => backend.read(path),
            Self::ReadOnly(backend) => backend.read(path),
            Self::ReadWrite(backend) => backend.read(path),
            Self::Overlay(backend) => backend.read(path),
        }
    }

    fn read_dir(
        &self,
        path: &VPath,
    ) -> io::Result<Vec<DirEntry>> {
        match self {
            Self::Mem(backend) => backend.read_dir(path),
            Self::ReadOnly(backend) => backend.read_dir(path),
            Self::ReadWrite(backend) => backend.read_dir(path),
            Self::Overlay(backend) => backend.read_dir(path),
        }
    }

    fn stat(
        &self,
        path: &VPath,
    ) -> io::Result<Metadata> {
        match self {
            Self::Mem(backend) => backend.stat(path),
            Self::ReadOnly(backend) => backend.stat(path),
            Self::ReadWrite(backend) => backend.stat(path),
            Self::Overlay(backend) => backend.stat(path),
        }
    }

    fn write(
        &self,
        path: &VPath,
        data: &[u8],
    ) -> io::Result<()> {
        match self {
            Self::Mem(backend) => backend.write(path, data),
            Self::ReadOnly(backend) => backend.write(path, data),
            Self::ReadWrite(backend) => backend.write(path, data),
            Self::Overlay(backend) => backend.write(path, data),
        }
    }

    fn mkdir(
        &self,
        path: &VPath,
    ) -> io::Result<()> {
        match self {
            Self::Mem(backend) => backend.mkdir(path),
            Self::ReadOnly(backend) => backend.mkdir(path),
            Self::ReadWrite(backend) => backend.mkdir(path),
            Self::Overlay(backend) => backend.mkdir(path),
        }
    }

    fn remove(
        &self,
        path: &VPath,
    ) -> io::Result<()> {
        match self {
            Self::Mem(backend) => backend.remove(path),
            Self::ReadOnly(backend) => backend.remove(path),
            Self::ReadWrite(backend) => backend.remove(path),
            Self::Overlay(backend) => backend.remove(path),
        }
    }

    fn rename(
        &self,
        from: &VPath,
        to: &VPath,
    ) -> io::Result<()> {
        match self {
            Self::Mem(backend) => backend.rename(from, to),
            Self::ReadOnly(backend) => backend.rename(from, to),
            Self::ReadWrite(backend) => backend.rename(from, to),
            Self::Overlay(backend) => backend.rename(from, to),
        }
    }
}

fn virtual_point(at: &Path) -> Result<VPath, SandboxError> {
    let point = at.to_str().and_then(VPath::new).ok_or_else(|| {
        SandboxError::InvalidPolicy(format!(
            "mount point {:?} is not a sandbox-absolute path",
            at.display().to_string()
        ))
    })?;
    Ok(point)
}

fn canonical_host(host: &Path) -> Result<std::path::PathBuf, SandboxError> {
    let canonical = host.canonicalize().map_err(|error| {
        SandboxError::InvalidPolicy(format!(
            "host directory {} is not accessible: {error}",
            host.display()
        ))
    })?;
    if !canonical.is_dir() {
        return Err(SandboxError::InvalidPolicy(format!(
            "{} is not a directory",
            host.display()
        )));
    }
    Ok(canonical)
}

fn build_backend(
    source: &MountSource,
    guard: &Arc<PathGuard>,
    budget: &Arc<ByteBudget>,
) -> Result<Backend, SandboxError> {
    let backend = match source {
        MountSource::Mem => Backend::Mem(Mem::new(guard.clone(), budget.clone())),
        MountSource::ReadOnly { host } => {
            Backend::ReadOnly(ReadOnlyMount::new(canonical_host(host)?, guard.clone()))
        },
        MountSource::ReadWrite { host } => Backend::ReadWrite(ReadWriteMount::new(
            canonical_host(host)?,
            guard.clone(),
            budget.clone(),
        )),
        MountSource::Overlay { host } => Backend::Overlay(Overlay::new(
            canonical_host(host)?,
            guard.clone(),
            budget.clone(),
        )),
    };
    Ok(backend)
}

#[cfg(test)]
mod tests {
    use super::MountedVfs;
    use crate::policy::{FsPolicy, Mount, MountSource};
    use crate::vfs::Vfs;
    use crate::vpath::VPath;
    use std::fs;
    use std::io::ErrorKind;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn temp_roots() -> (TempDir, PathBuf, PathBuf) {
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path().canonicalize().expect("canonical tempdir");
        let overlay_root = root.join("overlay");
        let write_root = root.join("write");
        fs::create_dir(&overlay_root).expect("overlay dir");
        fs::create_dir(&write_root).expect("write dir");
        (dir, overlay_root, write_root)
    }

    fn policy_with(
        overlay_root: &std::path::Path,
        write_root: &std::path::Path,
    ) -> FsPolicy {
        FsPolicy {
            mounts: vec![
                Mount {
                    at: PathBuf::from("/work"),
                    source: MountSource::Overlay {
                        host: overlay_root.to_path_buf(),
                    },
                },
                Mount {
                    at: PathBuf::from("/work/out"),
                    source: MountSource::ReadWrite {
                        host: write_root.to_path_buf(),
                    },
                },
                Mount {
                    at: PathBuf::from("/mem"),
                    source: MountSource::Mem,
                },
            ],
            ..FsPolicy::default()
        }
    }

    #[test]
    fn routes_to_the_longest_prefix_mount() {
        let (_dir, overlay_root, write_root) = temp_roots();
        let vfs = MountedVfs::from_policy(&policy_with(&overlay_root, &write_root))
            .expect("valid policy");

        vfs.mkdir(&VPath::new("//work/sub").expect("valid"))
            .expect("overlay mkdir");
        vfs.write(&VPath::new("//work/sub/f").expect("valid"), b"x")
            .expect("write");
        assert_eq!(
            vfs.read(&VPath::new("/work/sub/f").expect("valid"))
                .expect("read"),
            b"x".to_vec()
        );

        vfs.write(&VPath::new("//work/out/f").expect("valid"), b"host")
            .expect("nested write mount");
        assert!(write_root.join("f").exists());
        assert!(!overlay_root.join("out").exists());
    }

    #[test]
    fn unmounted_paths_are_absent() {
        let (_dir, overlay_root, write_root) = temp_roots();
        let vfs = MountedVfs::from_policy(&policy_with(&overlay_root, &write_root))
            .expect("valid policy");

        assert_eq!(
            vfs.stat(&VPath::new("//etc/passwd").expect("valid"))
                .expect_err("unmounted")
                .kind(),
            ErrorKind::NotFound
        );
        assert_eq!(
            vfs.stat(&VPath::root()).expect_err("no root mount").kind(),
            ErrorKind::NotFound
        );
    }

    #[test]
    fn listings_include_child_mount_points() {
        let (_dir, overlay_root, write_root) = temp_roots();
        let vfs = MountedVfs::from_policy(&policy_with(&overlay_root, &write_root))
            .expect("valid policy");

        let entries = vfs
            .read_dir(&VPath::new("/work").expect("valid"))
            .expect("listing");
        assert!(entries.iter().any(|entry| entry.name() == "out"));
    }

    #[test]
    fn cross_mount_rename_moves_files() {
        let (_dir, overlay_root, write_root) = temp_roots();
        let vfs = MountedVfs::from_policy(&policy_with(&overlay_root, &write_root))
            .expect("valid policy");
        let from = VPath::new("//work/file.txt").expect("valid");
        let to = VPath::new("//mem/file.txt").expect("valid");

        vfs.write(&from, b"cross").expect("write");
        vfs.rename(&from, &to).expect("cross-mount rename");
        assert!(vfs.stat(&from).is_err());
        assert_eq!(vfs.read(&to).expect("read"), b"cross".to_vec());

        let dir = VPath::new("//work/subdir").expect("valid");
        vfs.mkdir(&dir).expect("mkdir");
        assert_eq!(
            vfs.rename(&dir, &VPath::new("//mem/subdir").expect("valid"))
                .expect_err("cross-mount directory rename")
                .kind(),
            ErrorKind::InvalidInput
        );
    }

    #[test]
    fn invalid_mounts_fail_closed() {
        let dir = TempDir::new().expect("tempdir");
        let missing_host = dir.path().join("missing");

        let relative_mount = FsPolicy {
            mounts: vec![Mount {
                at: PathBuf::from("relative"),
                source: MountSource::Mem,
            }],
            ..FsPolicy::default()
        };
        assert!(MountedVfs::from_policy(&relative_mount).is_err());

        let missing_host_mount = FsPolicy {
            mounts: vec![Mount {
                at: PathBuf::from("/gone"),
                source: MountSource::ReadOnly { host: missing_host },
            }],
            ..FsPolicy::default()
        };
        assert!(MountedVfs::from_policy(&missing_host_mount).is_err());

        let (overlay_root, write_root) = (dir.path().join("a"), dir.path().join("b"));
        fs::create_dir(&overlay_root).expect("a");
        fs::create_dir(&write_root).expect("b");
        let duplicate = FsPolicy {
            mounts: vec![
                Mount {
                    at: PathBuf::from("/x"),
                    source: MountSource::Mem,
                },
                Mount {
                    at: PathBuf::from("/x"),
                    source: MountSource::Mem,
                },
            ],
            ..FsPolicy::default()
        };
        assert!(MountedVfs::from_policy(&duplicate).is_err());
        let _ = (overlay_root, write_root);
    }

    #[test]
    fn empty_policy_mounts_nothing() {
        let vfs = MountedVfs::from_policy(&FsPolicy::default()).expect("empty policy");

        assert_eq!(
            vfs.read(&VPath::root().join("anything"))
                .expect_err("nothing mounted")
                .kind(),
            ErrorKind::NotFound
        );
    }
}
