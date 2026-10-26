//! One writer for one store.
//!
//! The daemon is the only process that appends to the log, and that is a
//! property of the design rather than of the filesystem: SQLite serialises
//! concurrent writers instead of refusing them, so two daemons on one state
//! directory both append, both fold, and both decide what to do about the same
//! turn. The log stays valid and the meaning of it does not.
//!
//! The lock is an advisory `flock` on a file beside the store. The kernel drops
//! it when the process ends, including a process that was killed, so there is no
//! stale lock to clear and no pid to trust.

use std::fmt::{self, Display, Formatter};
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use rustix::fs::FlockOperation;
use rustix::io::Errno;

/// The file the daemon locks, beside the store it writes.
pub const LOCK_FILE: &str = "agent.lock";

/// Why a daemon could not claim the store.
#[derive(Debug)]
pub enum LockError {
    /// Another process holds the lock on this state directory.
    Busy(PathBuf),
    /// The lock file could not be opened or locked.
    Io(PathBuf, std::io::Error),
}

impl Display for LockError {
    fn fmt(
        &self,
        formatter: &mut Formatter<'_>,
    ) -> fmt::Result {
        match self {
            Self::Busy(path) => write!(
                formatter,
                "another agentd is already writing this state: {} is locked",
                path.display()
            ),
            Self::Io(path, error) => {
                write!(formatter, "cannot lock {}: {error}", path.display())
            },
        }
    }
}

impl std::error::Error for LockError {}

/// A held lock, released when this value is dropped.
#[derive(Debug)]
pub struct Lock {
    /// Kept open because closing is what releases the lock.
    ///
    /// The file is never removed. Two processes that each remove and recreate the
    /// same path would hold locks on two different files and both believe they
    /// were alone, which is the bug this exists to prevent.
    _file: File,
}

impl Lock {
    /// Takes the lock for a state directory, or says who has it.
    ///
    /// # Errors
    ///
    /// Returns [`LockError::Busy`] when another process holds it, and
    /// [`LockError::Io`] when the file cannot be opened or locked.
    pub fn take(dir: &Path) -> Result<Self, LockError> {
        let path = dir.join(LOCK_FILE);
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(|error| LockError::Io(path.clone(), error))?;
        match rustix::fs::flock(&file, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => Ok(Self { _file: file }),
            Err(Errno::WOULDBLOCK) => Err(LockError::Busy(path)),
            Err(error) => Err(LockError::Io(
                path,
                std::io::Error::from_raw_os_error(error.raw_os_error()),
            )),
        }
    }
}

#[cfg(test)]
mod tests;
