//! Durable per-subscriber cursors.
//!
//! Implements the cursor store from `docs/event-bus/delivery.md`. A cursor is
//! the last sequence number a subscriber has confirmed processing. It is stored
//! in `cursors/{uid}/{subscriber_id}.cursor` and replaced atomically, so a crash
//! during a write cannot leave a half-written cursor.
//!
//! The subscriber ID is client-supplied, so it is validated before it becomes a
//! path component: an ID containing a path separator or `..` is rejected rather
//! than allowed to escape the cursor directory.
//!
//! ```
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! use agent::cursor::{CursorKey, CursorStore};
//!
//! let dir = std::env::temp_dir().join(format!("agent-cursor-doc-{}", std::process::id()));
//! let _ = std::fs::remove_dir_all(&dir);
//!
//! let store = CursorStore::open(&dir)?;
//! let key = CursorKey::new(1000, "audit-log");
//!
//! assert_eq!(store.load(&key)?, None);
//! store.store(&key, 1024)?;
//! assert_eq!(store.load(&key)?, Some(1024));
//! # std::fs::remove_dir_all(&dir)?;
//! # Ok(())
//! # }
//! ```

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// The stable identity of a subscriber's cursor: a client-supplied ID namespaced
/// by the peer UID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorKey {
    /// The peer's user ID, which namespaces the cursor.
    pub uid: u32,
    /// The client-supplied subscriber ID.
    pub subscriber_id: String,
}

impl CursorKey {
    /// Build a key, without validating the subscriber ID yet.
    #[must_use]
    pub fn new(
        uid: u32,
        subscriber_id: impl Into<String>,
    ) -> Self {
        Self {
            uid,
            subscriber_id: subscriber_id.into(),
        }
    }

    /// The validated file name for this key's cursor.
    fn file_name(&self) -> Result<String, CursorError> {
        validate_subscriber_id(&self.subscriber_id)?;
        Ok(format!("{}.cursor", self.subscriber_id))
    }
}

/// The longest subscriber ID accepted, to bound a path component.
const MAX_SUBSCRIBER_ID_LEN: usize = 128;

/// Reject a subscriber ID that would be an unsafe or ambiguous path component.
fn validate_subscriber_id(id: &str) -> Result<(), CursorError> {
    let safe_chars = id
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));
    if id.is_empty() || id.len() > MAX_SUBSCRIBER_ID_LEN || !safe_chars || id == "." || id == ".." {
        return Err(CursorError::InvalidSubscriberId(id.to_owned()));
    }
    Ok(())
}

/// A directory of durable cursor files.
#[derive(Debug, Clone)]
pub struct CursorStore {
    dir: PathBuf,
}

impl CursorStore {
    /// Open a cursor store rooted at `dir`, creating it if absent.
    ///
    /// # Errors
    ///
    /// Returns a [`CursorError`] if the directory cannot be created.
    pub fn open(dir: impl AsRef<Path>) -> Result<Self, CursorError> {
        let dir = dir.as_ref().to_path_buf();
        fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    /// The directory that holds the cursors.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Load a subscriber's cursor, or `None` if it has none yet.
    ///
    /// A missing file is not an error; it means the subscriber has never
    /// acknowledged anything.
    ///
    /// # Errors
    ///
    /// Returns a [`CursorError`] if the ID is invalid, the file cannot be read,
    /// or its contents are not a valid sequence number.
    pub fn load(
        &self,
        key: &CursorKey,
    ) -> Result<Option<u64>, CursorError> {
        let path = self.path_for(key)?;
        match fs::read_to_string(&path) {
            Ok(text) => {
                let trimmed = text.trim();
                trimmed
                    .parse::<u64>()
                    .map(Some)
                    .map_err(|_| CursorError::Corrupt {
                        path,
                        value: trimmed.to_owned(),
                    })
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// Durably record a subscriber's cursor.
    ///
    /// The cursor is written to a temporary file, `fsync`ed, and renamed over
    /// the real file, so a reader sees either the old cursor or the new one,
    /// never a partial write.
    ///
    /// # Errors
    ///
    /// Returns a [`CursorError`] if the ID is invalid or the write fails.
    pub fn store(
        &self,
        key: &CursorKey,
        cursor: u64,
    ) -> Result<(), CursorError> {
        let path = self.path_for(key)?;
        let dir = match path.parent() {
            Some(parent) => parent.to_path_buf(),
            None => {
                return Err(CursorError::Io(io::Error::other(
                    "cursor path has no parent",
                )));
            },
        };
        fs::create_dir_all(&dir)?;

        let temp = dir.join(format!(".{}.tmp", key.subscriber_id));
        {
            let mut file = File::create(&temp)?;
            writeln!(file, "{cursor}")?;
            file.sync_all()?;
        }
        fs::rename(&temp, &path)?;
        // The rename must reach stable storage, so a crash cannot revert it.
        fs::File::open(&dir)?.sync_all()?;
        Ok(())
    }

    /// The sequence a subscriber should resume from.
    ///
    /// Per `docs/event-bus/delivery.md`, this is `max(requested_from_seq,
    /// stored_cursor)`, so a resume never silently skips events. Re-delivering
    /// the last acknowledged sequence is expected: delivery is at-least-once and
    /// consumers deduplicate.
    ///
    /// # Errors
    ///
    /// Returns a [`CursorError`] if the stored cursor cannot be read.
    pub fn resolve(
        &self,
        key: &CursorKey,
        requested_from_seq: u64,
    ) -> Result<u64, CursorError> {
        Ok(requested_from_seq.max(self.load(key)?.unwrap_or(0)))
    }

    /// The full path of a subscriber's cursor file.
    fn path_for(
        &self,
        key: &CursorKey,
    ) -> Result<PathBuf, CursorError> {
        Ok(self
            .dir
            .join(format!("{:08x}", key.uid))
            .join(key.file_name()?))
    }
}

/// Failures of the cursor store.
#[derive(Debug, thiserror::Error)]
pub enum CursorError {
    /// A filesystem operation failed.
    #[error("cursor I/O error: {0}")]
    Io(#[from] io::Error),
    /// A subscriber ID was empty, too long, or unsafe as a path component.
    #[error("invalid subscriber id `{0}`")]
    InvalidSubscriberId(String),
    /// A cursor file did not contain a sequence number.
    #[error("cursor file `{path}` is corrupt: {value}")]
    Corrupt {
        /// The offending file.
        path: PathBuf,
        /// The contents that could not be parsed.
        value: String,
    },
}

#[cfg(test)]
mod tests;
