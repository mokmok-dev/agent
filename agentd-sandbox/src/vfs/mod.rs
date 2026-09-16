//! The virtual filesystem: the single plane every execution path shares.
//!
//! The [`Vfs`] trait is the only filesystem surface inside the sandbox: path
//! confinement (normalization, `..` clamping, symlink-escape prevention) is
//! applied uniformly here, and the refuse/hide globs and byte caps of the
//! [`FsPolicy`](crate::policy::FsPolicy) hold across every implementation.

// The in-memory backends hold short-lived mutex guards across fallible
// checks. Satisfying `significant_drop_tightening` would mean splitting every
// operation into a decision pass and an effect pass under two locks, which
// trades a real correctness property (atomic check-and-apply) for shorter
// guard lifetimes that do not matter here: the guards protect small in-memory
// maps, the lock is never held across an await, and layer 1 processes do not
// touch the VFS at all.
#![allow(clippy::significant_drop_tightening)]

mod guard;
mod mem;
mod mount;
mod overlay;
mod router;
mod tree;

use std::io;

use crate::vpath::VPath;

pub use router::MountedVfs;

/// The filesystem surface inside the sandbox.
///
/// Every method takes a [`VPath`] sandbox-absolute path and an implementation
/// routes it to the mount that covers it. Writing to a read-only mount,
/// escaping a mount through a symlink, or touching a refused path fails with
/// the corresponding I/O error kind.
pub trait Vfs: Send + Sync {
    /// Reads a file's bytes.
    ///
    /// # Errors
    ///
    /// Reading a directory fails with [`io::ErrorKind::IsADirectory`]; a
    /// refused path fails with [`io::ErrorKind::PermissionDenied`]; a hidden
    /// or absent path fails with [`io::ErrorKind::NotFound`].
    fn read(
        &self,
        path: &VPath,
    ) -> io::Result<Vec<u8>>;

    /// Lists a directory's visible entries. Hidden entries are skipped.
    ///
    /// # Errors
    ///
    /// Fails with [`io::ErrorKind::NotADirectory`] for files,
    /// [`io::ErrorKind::PermissionDenied`] for refused paths, and
    /// [`io::ErrorKind::NotFound`] for hidden or absent paths.
    fn read_dir(
        &self,
        path: &VPath,
    ) -> io::Result<Vec<DirEntry>>;

    /// Describes a file or directory.
    ///
    /// # Errors
    ///
    /// Fails with [`io::ErrorKind::PermissionDenied`] for refused paths and
    /// [`io::ErrorKind::NotFound`] for hidden or absent paths.
    fn stat(
        &self,
        path: &VPath,
    ) -> io::Result<Metadata>;

    /// Creates or replaces a file. The parent directory must exist.
    ///
    /// # Errors
    ///
    /// Fails with [`io::ErrorKind::NotFound`] for missing parents,
    /// [`io::ErrorKind::IsADirectory`] for directory targets, byte-cap
    /// exceedance with [`io::ErrorKind::FileTooLarge`] or
    /// [`io::ErrorKind::StorageFull`], and
    /// [`io::ErrorKind::PermissionDenied`] on read-only mounts or refused
    /// paths.
    fn write(
        &self,
        path: &VPath,
        data: &[u8],
    ) -> io::Result<()>;

    /// Creates a directory. The parent directory must exist.
    ///
    /// # Errors
    ///
    /// Fails with [`io::ErrorKind::AlreadyExists`] for existing paths,
    /// [`io::ErrorKind::NotFound`] for missing parents, and
    /// [`io::ErrorKind::PermissionDenied`] on read-only mounts or refused
    /// paths.
    fn mkdir(
        &self,
        path: &VPath,
    ) -> io::Result<()>;

    /// Removes a file or an empty directory.
    ///
    /// # Errors
    ///
    /// Fails with [`io::ErrorKind::DirectoryNotEmpty`] for non-empty
    /// directories and [`io::ErrorKind::PermissionDenied`] on read-only
    /// mounts or refused paths.
    fn remove(
        &self,
        path: &VPath,
    ) -> io::Result<()>;

    /// Renames a file.
    ///
    /// # Errors
    ///
    /// Fails with [`io::ErrorKind::InvalidInput`] for directories and mount
    /// roots, and [`io::ErrorKind::PermissionDenied`] on read-only mounts or
    /// refused paths.
    fn rename(
        &self,
        from: &VPath,
        to: &VPath,
    ) -> io::Result<()>;
}

/// One entry of a directory listing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirEntry {
    /// The entry's file name, not the full path.
    pub name: String,
    /// Whether the entry is a directory.
    pub is_dir: bool,
}

impl DirEntry {
    /// The entry's file name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
}

/// The result of [`Vfs::stat`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Metadata {
    /// Whether the path is a directory.
    pub is_dir: bool,
    /// The file's length in bytes; directories report zero.
    pub len: u64,
}
