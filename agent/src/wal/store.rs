//! File-backed, segmented WAL store.
//!
//! Implements the on-disk layout and write path from `docs/event-bus/wal.md`:
//! append-only segment files, a single writer that assigns sequence numbers and
//! links each record into the BLAKE3 chain, group commit, and crash recovery.
//!
//! Layout:
//!
//! ```text
//! <dir>/
//!   segment-{first_seq:020}.log
//! ```
//!
//! Segment names sort lexicographically in sequence order. Rotation is not
//! deletion: every segment is retained. Only the last segment is open for
//! append.
//!
//! [`Store::append_batch`] writes all the given records and `fsync`s once for
//! the batch. A single `append` is the degenerate one-record batch. Durability
//! is the same either way: a record is acknowledged only after its bytes reach
//! stable storage.
//!
//! The store is synchronous. Group commit is the primitive here; the concurrent
//! writer that drives it with batching arrives with the transport milestone.
//!
//! ```
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! use agent::wal::Store;
//!
//! let dir = std::env::temp_dir().join(format!("agent-store-doc-{}", std::process::id()));
//! let _ = std::fs::remove_dir_all(&dir);
//!
//! let mut store = Store::open(&dir)?;
//! let first = store.append(b"hello")?;
//! let batch = store.append_batch(&[b"world".as_slice(), b"!".as_slice()])?;
//! assert_eq!(first.seq, 0);
//! assert_eq!(batch[0].seq, 1);
//! assert_eq!(store.head_seq(), Some(2));
//!
//! // Reopening recovers the same head from disk.
//! let reopened = Store::open(&dir)?;
//! assert_eq!(reopened.head_seq(), Some(2));
//! # std::fs::remove_dir_all(&dir)?;
//! # Ok(())
//! # }
//! ```

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::wal::chain::{HASH_LEN, genesis_hash, hash_record};
use crate::wal::error::Error;
use crate::wal::frame::Record;
use crate::wal::replay::Replay;
use crate::wal::scan::recover_from;

/// Default target size of a segment file: 64 MiB.
pub const DEFAULT_SEGMENT_SIZE: u64 = 64 * 1024 * 1024;

/// A record committed to stable storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Committed {
    /// The sequence number the store assigned.
    pub seq: u64,
    /// The commit timestamp in nanoseconds since the Unix epoch.
    pub timestamp_ns: u64,
}

/// One segment found on disk and the result of validating it.
#[derive(Debug)]
struct ScannedSegment {
    path: PathBuf,
    total_len: u64,
    committed_len: u64,
    records: u64,
}

/// The validated state of a store directory.
#[derive(Debug)]
struct Scan {
    segments: Vec<ScannedSegment>,
    head_hash: [u8; HASH_LEN],
    next_seq: u64,
    /// The last segment's path and committed length when it ends in a torn
    /// record that recovery must truncate.
    torn: Option<(PathBuf, u64)>,
}

impl Scan {
    /// The sequence number of the last committed record, if any.
    const fn head_seq(&self) -> Option<u64> {
        self.next_seq.checked_sub(1)
    }
}

/// Failures of the file-backed store.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// A filesystem operation failed.
    #[error("store I/O error: {0}")]
    Io(#[from] io::Error),
    /// A segment failed frame or chain validation.
    #[error("segment `{path}` failed validation: {source}")]
    Segment {
        /// The offending segment file.
        path: PathBuf,
        /// The frame or chain failure.
        #[source]
        source: Error,
    },
    /// A record could not be encoded, for example because its payload exceeds
    /// the 32-bit length field.
    #[error("could not encode a record: {0}")]
    Encode(#[from] Error),
    /// An earlier write failed, leaving unacknowledged bytes on disk.
    ///
    /// Reopening the store truncates the torn record and clears this.
    #[error("store is poisoned by an earlier write failure; reopen it")]
    Poisoned,
    /// A segment that is not the last one ends in a torn record.
    ///
    /// Only the last segment may be truncated by recovery; a torn record in an
    /// earlier segment means the log was damaged after it was committed.
    #[error("non-final segment `{path}` ends in a torn record")]
    TornNonFinalSegment {
        /// The offending segment file.
        path: PathBuf,
    },
}

/// The open append segment.
#[derive(Debug)]
struct SegmentWriter {
    file: File,
    len: u64,
}

/// A single-writer, append-only, segmented WAL store.
#[derive(Debug)]
pub struct Store {
    dir: PathBuf,
    target_segment_size: u64,
    next_seq: u64,
    head_hash: [u8; HASH_LEN],
    segment: Option<SegmentWriter>,
    /// Set once a write leaves bytes on disk that are not an acknowledged
    /// record.
    ///
    /// Continuing to append after a mid-write failure could interleave a new
    /// record with partial bytes, and rotating would make those partial bytes a
    /// non-final torn record, which recovery treats as fatal. The store is
    /// therefore poisoned: reads and a reopen still work, but every further
    /// append fails until the process reopens it.
    poisoned: bool,
    /// Test-only switch that makes the next write fail, so the poison path is
    /// reachable without a real I/O fault.
    #[cfg(test)]
    fail_next_write: bool,
}

impl Store {
    /// Open a store in `dir` with the default segment size, recovering any
    /// existing log.
    ///
    /// Creates `dir` if it does not exist, truncates a torn trailing record in
    /// the last segment, and refuses to open a log that fails validation
    /// anywhere else.
    ///
    /// # Errors
    ///
    /// Returns a [`StoreError`] if the directory cannot be read or created, or
    /// if any committed record fails checksum or chain validation.
    pub fn open(dir: impl AsRef<Path>) -> Result<Self, StoreError> {
        Self::with_segment_size(dir, DEFAULT_SEGMENT_SIZE)
    }

    /// Open a store with an explicit segment rotation target, in bytes.
    ///
    /// The target is an approximation: a record is never split across segments,
    /// so a segment may exceed the target by up to one record.
    ///
    /// # Errors
    ///
    /// As [`Store::open`].
    pub fn with_segment_size(
        dir: impl AsRef<Path>,
        target_segment_size: u64,
    ) -> Result<Self, StoreError> {
        let dir = dir.as_ref();
        fs::create_dir_all(dir)?;

        let scan = scan_dir(dir)?;

        // Truncating a torn trailing record is the only place the log is
        // shortened, and it removes only bytes that were never acknowledged.
        if let Some((path, committed_len)) = &scan.torn {
            let file = OpenOptions::new().write(true).open(path)?;
            file.set_len(*committed_len)?;
            file.sync_all()?;
        }

        let segment = match scan.segments.last() {
            Some(last) => {
                let file = OpenOptions::new().append(true).open(&last.path)?;
                Some(SegmentWriter {
                    file,
                    len: last.committed_len,
                })
            },
            None => None,
        };

        Ok(Self {
            dir: dir.to_path_buf(),
            target_segment_size,
            next_seq: scan.next_seq,
            head_hash: scan.head_hash,
            segment,
            poisoned: false,
            #[cfg(test)]
            fail_next_write: false,
        })
    }

    /// Append one record and commit it.
    ///
    /// # Errors
    ///
    /// Returns a [`StoreError`] if the record cannot be encoded or written, or
    /// if the store was poisoned by an earlier write failure.
    pub fn append(
        &mut self,
        payload: &[u8],
    ) -> Result<Committed, StoreError> {
        let mut committed = self.append_batch(&[payload])?;
        committed
            .pop()
            .ok_or_else(|| StoreError::Io(io::Error::other("empty batch")))
    }

    /// Append a batch of records and commit the batch with a single `fsync`.
    ///
    /// This is the group-commit primitive: the records are assigned consecutive
    /// sequence numbers, written, and flushed to stable storage. A segment
    /// rotation inside the batch costs one extra `fsync` for the part already
    /// written. Sequence assignment is contiguous across the batch.
    ///
    /// # Errors
    ///
    /// Returns a [`StoreError`] if a record cannot be encoded or written, or if
    /// the store was poisoned by an earlier write failure. On failure the
    /// in-memory committed state is left unchanged and the store is poisoned;
    /// the partial bytes on disk are a torn trailing record that the next
    /// [`Store::open`] truncates.
    pub fn append_batch(
        &mut self,
        payloads: &[&[u8]],
    ) -> Result<Vec<Committed>, StoreError> {
        if self.poisoned {
            return Err(StoreError::Poisoned);
        }
        if payloads.is_empty() {
            return Ok(Vec::new());
        }

        // Phase 1 is pure computation: assign sequence numbers, link the chain,
        // and encode. An encode failure writes nothing, so it does not poison
        // the store.
        let (committed, encoded) = self.encode_batch(payloads)?;

        // Phase 2 writes to disk. A failure here can leave partial bytes, so the
        // store is poisoned and must be reopened.
        if let Err(error) = self.write_batch(&encoded, &committed) {
            self.poisoned = true;
            return Err(error);
        }
        Ok(committed)
    }

    /// Assign sequence numbers, link the chain, and encode a batch.
    ///
    /// Nothing is written, so this does not mutate the store's committed state.
    fn encode_batch(
        &self,
        payloads: &[&[u8]],
    ) -> Result<(Vec<Committed>, Vec<Vec<u8>>), StoreError> {
        let mut seq = self.next_seq;
        let mut hash = self.head_hash;
        let mut committed = Vec::with_capacity(payloads.len());
        let mut encoded = Vec::with_capacity(payloads.len());

        for payload in payloads {
            let timestamp_ns = now_ns();
            let record = Record::new(seq, timestamp_ns, payload.to_vec(), hash);
            let bytes = record.encode()?;
            hash = hash_record(&bytes);
            committed.push(Committed { seq, timestamp_ns });
            encoded.push(bytes);
            seq = seq.saturating_add(1);
        }

        Ok((committed, encoded))
    }

    /// Write an encoded batch and `fsync` it once, rotating as needed.
    ///
    /// Updates the committed state only after the final `fsync` succeeds.
    fn write_batch(
        &mut self,
        encoded: &[Vec<u8>],
        committed: &[Committed],
    ) -> Result<(), StoreError> {
        if self.segment.is_none() {
            self.open_segment(self.next_seq)?;
        }

        let mut pending: Vec<u8> = Vec::new();
        let mut last_hash = self.head_hash;
        for (bytes, entry) in encoded.iter().zip(committed) {
            let current = self.segment.as_ref().map_or(0, |segment| segment.len);
            let effective = current + pending.len() as u64;
            if effective > 0 && effective + bytes.len() as u64 > self.target_segment_size {
                self.write_and_sync(&pending)?;
                pending.clear();
                self.roll(entry.seq)?;
            }
            pending.extend_from_slice(bytes);
            last_hash = hash_record(bytes);
        }

        self.write_and_sync(&pending)?;
        if let Some(last) = committed.last() {
            self.next_seq = last.seq.saturating_add(1);
        }
        self.head_hash = last_hash;
        Ok(())
    }

    /// Replay committed records from `from_seq`, in sequence order.
    ///
    /// The returned iterator reads one segment at a time, so it does not hold
    /// more than one segment in memory. Records with a sequence below
    /// `from_seq` are skipped, including those in the segment that contains it.
    ///
    /// Replay is for delivery, not recovery: the store validated the log at
    /// [`Store::open`], and the caller decodes each payload as a `CloudEvents`
    /// envelope. The `from_seq` of a subscriber is `max(requested, cursor)`; see
    /// [`crate::cursor::CursorStore::resolve`].
    ///
    /// # Errors
    ///
    /// Returns a [`StoreError`] if the directory cannot be listed. Errors while
    /// reading an individual segment are yielded from the iterator.
    pub fn replay(
        &self,
        from_seq: u64,
    ) -> Result<Replay, StoreError> {
        Replay::new(&self.dir, from_seq)
    }

    /// The sequence number of the last committed record, if any.
    ///
    /// Sequence numbers are assigned contiguously from zero, so this is
    /// `next_seq - 1`.
    #[must_use]
    pub const fn head_seq(&self) -> Option<u64> {
        self.next_seq.checked_sub(1)
    }

    /// The chain hash of the last committed record, or the genesis hash when
    /// the log is empty.
    ///
    /// The next record links to this value.
    #[must_use]
    pub const fn head_hash(&self) -> [u8; HASH_LEN] {
        self.head_hash
    }

    /// The directory that holds the segments.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The configured segment rotation target in bytes.
    #[must_use]
    pub const fn segment_size(&self) -> u64 {
        self.target_segment_size
    }

    /// Write `bytes` to the open segment and `fsync` them.
    fn write_and_sync(
        &mut self,
        bytes: &[u8],
    ) -> Result<(), StoreError> {
        if bytes.is_empty() {
            return Ok(());
        }
        #[cfg(test)]
        if std::mem::take(&mut self.fail_next_write) {
            return Err(StoreError::Io(io::Error::other("injected write failure")));
        }
        let Some(segment) = self.segment.as_mut() else {
            return Err(StoreError::Io(io::Error::other("no open segment")));
        };
        segment.file.write_all(bytes)?;
        segment.file.sync_all()?;
        segment.len += bytes.len() as u64;
        Ok(())
    }

    /// Close the open segment and open a fresh one whose first record is
    /// `first_seq`.
    fn roll(
        &mut self,
        first_seq: u64,
    ) -> Result<(), StoreError> {
        self.segment = None;
        self.open_segment(first_seq)
    }

    /// Create and open the segment for the record with sequence `first_seq`.
    fn open_segment(
        &mut self,
        first_seq: u64,
    ) -> Result<(), StoreError> {
        let path = self.dir.join(segment_name(first_seq));
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let len = file.metadata()?.len();
        self.segment = Some(SegmentWriter { file, len });
        // The new directory entry must reach stable storage, or a crash could
        // drop a segment whose records were already acknowledged.
        sync_dir(&self.dir)?;
        Ok(())
    }
}

/// A summary of a store directory produced by [`verify_dir`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyReport {
    /// The number of segment files.
    pub segments: u64,
    /// The number of validated records.
    pub records: u64,
    /// Bytes of the log covered by validated records.
    pub committed_len: u64,
    /// Total bytes across all segments.
    pub total_len: u64,
    /// The sequence number of the last validated record, if any.
    pub head_seq: Option<u64>,
    /// Whether every segment was wholly covered by validated records.
    ///
    /// When false, the last segment ends in a torn record that recovery would
    /// truncate.
    pub is_clean: bool,
}

/// Validate every segment in `dir` without modifying it.
///
/// Walks the segments in sequence order, recomputing each record's `crc32c` and
/// the BLAKE3 chain across segment boundaries. Reports the first failure and
/// whether the last segment ends in a torn record.
///
/// # Errors
///
/// Returns a [`StoreError`] if a segment cannot be read or a committed record
/// fails validation.
pub fn verify_dir(dir: impl AsRef<Path>) -> Result<VerifyReport, StoreError> {
    let scan = scan_dir(dir.as_ref())?;
    Ok(VerifyReport {
        segments: scan.segments.len() as u64,
        records: scan.segments.iter().map(|segment| segment.records).sum(),
        committed_len: scan
            .segments
            .iter()
            .map(|segment| segment.committed_len)
            .sum(),
        total_len: scan.segments.iter().map(|segment| segment.total_len).sum(),
        head_seq: scan.head_seq(),
        is_clean: scan.torn.is_none(),
    })
}

/// Validate a store directory and report the first failure.
fn scan_dir(dir: &Path) -> Result<Scan, StoreError> {
    let mut files = segment_files(dir)?;
    files.sort_by_key(|(first_seq, _)| *first_seq);

    let last_index = files.len().saturating_sub(1);
    let mut segments = Vec::with_capacity(files.len());
    let mut prev_hash = genesis_hash();
    let mut next_seq = 0u64;
    let mut torn = None;

    for (index, (_first_seq, path)) in files.into_iter().enumerate() {
        let bytes = fs::read(&path)?;
        let recovery = recover_from(&bytes, prev_hash).map_err(|source| StoreError::Segment {
            path: path.clone(),
            source,
        })?;

        if !recovery.is_clean() {
            if index == last_index {
                torn = Some((path.clone(), recovery.committed_len() as u64));
            } else {
                return Err(StoreError::TornNonFinalSegment { path });
            }
        }

        if let Some(entry) = recovery.records().last() {
            let start = entry.offset();
            let end = start + entry.byte_len();
            prev_hash = hash_record(&bytes[start..end]);
            next_seq = entry.seq().saturating_add(1);
        }

        segments.push(ScannedSegment {
            path,
            total_len: bytes.len() as u64,
            committed_len: recovery.committed_len() as u64,
            records: recovery.records().len() as u64,
        });
    }

    Ok(Scan {
        segments,
        head_hash: prev_hash,
        next_seq,
        torn,
    })
}

/// List the segment files in `dir`, excluding anything that is not named as a
/// segment.
pub(super) fn segment_files(dir: &Path) -> Result<Vec<(u64, PathBuf)>, StoreError> {
    let mut files = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if let Some(first_seq) = parse_segment_name(name) {
            files.push((first_seq, path));
        }
    }
    Ok(files)
}

/// The file name for the segment whose first record has sequence `first_seq`.
fn segment_name(first_seq: u64) -> String {
    format!("segment-{first_seq:020}.log")
}

/// Parse a segment file name back to its first sequence number.
///
/// Returns `None` for anything that is not `segment-<20 digits>.log`.
fn parse_segment_name(name: &str) -> Option<u64> {
    let digits = name.strip_prefix("segment-")?.strip_suffix(".log")?;
    if digits.len() != 20 || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// `fsync` a directory so new or removed entries are durable.
fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

/// The current time in nanoseconds since the Unix epoch, saturating at
/// `u64::MAX`.
fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX)
        })
}

#[cfg(test)]
mod tests;
