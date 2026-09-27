//! Replay of committed records from the log.
//!
//! [`Replay`] reads records back from a starting sequence number, one segment at
//! a time, so replaying a long history holds at most one segment in memory
//! rather than the whole log. The store already validated framing and the hash
//! chain when it was opened (see [`crate::wal::Store::open`]), so replay decodes
//! records without re-walking the chain; each record's `crc32c` is still
//! checked.
//!
//! Replay is byte-oriented: it yields a record's payload, and the caller decodes
//! the `CloudEvents` envelope. This keeps the WAL independent of the event
//! model.

use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};

use crate::wal::frame::{HEADER_LEN, Record, parse_header, record_len};
use crate::wal::store::{StoreError, segment_files};

/// A committed record read back from the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayedRecord {
    /// The record's sequence number.
    pub seq: u64,
    /// The commit timestamp in nanoseconds since the Unix epoch.
    pub timestamp_ns: u64,
    /// The record's payload bytes.
    pub payload: Vec<u8>,
}

/// A lazy reader over committed records from a start sequence.
///
/// Produced by [`crate::wal::Store::replay`]. Each [`Iterator::next`] yields one
/// record in sequence order, or a [`StoreError`] if a segment cannot be read.
/// After an error, the iterator ends.
#[derive(Debug)]
pub struct Replay {
    segments: VecDeque<PathBuf>,
    current: VecDeque<ReplayedRecord>,
    from_seq: u64,
    finished: bool,
}

impl Replay {
    /// Build a replay over `dir` starting at `from_seq`.
    ///
    /// # Errors
    ///
    /// Returns a [`StoreError`] if the directory cannot be listed.
    pub(crate) fn new(
        dir: &Path,
        from_seq: u64,
    ) -> Result<Self, StoreError> {
        let mut files = segment_files(dir)?;
        files.sort_by_key(|(first_seq, _)| *first_seq);

        // Start at the segment that may contain `from_seq`: the last segment
        // whose first sequence is at or below it, or the first segment when
        // `from_seq` precedes all of them. An empty store yields nothing.
        let start = match files
            .iter()
            .position(|(first_seq, _)| *first_seq > from_seq)
        {
            Some(0) => 0,
            Some(index) => index - 1,
            None => files.len().saturating_sub(1),
        };

        Ok(Self {
            segments: files
                .into_iter()
                .skip(start)
                .map(|(_, path)| path)
                .collect(),
            current: VecDeque::new(),
            from_seq,
            finished: false,
        })
    }
}

impl Iterator for Replay {
    type Item = Result<ReplayedRecord, StoreError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(record) = self.current.pop_front() {
                return Some(Ok(record));
            }
            if self.finished {
                return None;
            }
            let Some(path) = self.segments.pop_front() else {
                self.finished = true;
                return None;
            };
            match read_segment(&path) {
                Ok(records) => {
                    // The segment that contains `from_seq` also holds earlier
                    // records, which must not be delivered.
                    let from_seq = self.from_seq;
                    self.current = records
                        .into_iter()
                        .filter(|record| record.seq >= from_seq)
                        .collect();
                },
                Err(error) => {
                    self.finished = true;
                    return Some(Err(error));
                },
            }
        }
    }
}

/// Decode every whole record in a segment's bytes.
///
/// The store validated the segment's chain at open, so this checks only framing
/// and each record's `crc32c`. A torn trailing record — a prefix of a record at
/// the end — is not an error: it ends the scan, matching recovery's rule that
/// only the last segment may end in a partial write. The store truncates that
/// tail on open, so this is defensive.
fn decode_segment(
    path: &Path,
    mut bytes: &[u8],
) -> Result<Vec<ReplayedRecord>, StoreError> {
    let mut records = Vec::new();

    while !bytes.is_empty() {
        // A partial fixed header can only be a torn trailing write.
        if bytes.len() < HEADER_LEN {
            break;
        }
        let header = parse_header(bytes).map_err(|source| StoreError::Segment {
            path: path.to_path_buf(),
            source,
        })?;
        let Some(len) = record_len(header.payload_len as usize) else {
            break;
        };
        if bytes.len() < len {
            break;
        }
        let (record_bytes, rest) = bytes.split_at(len);
        let record = Record::decode(record_bytes).map_err(|source| StoreError::Segment {
            path: path.to_path_buf(),
            source,
        })?;
        records.push(ReplayedRecord {
            seq: record.seq(),
            timestamp_ns: record.timestamp_ns(),
            payload: record.payload().to_vec(),
        });
        bytes = rest;
    }

    Ok(records)
}

/// Read and decode a segment file.
fn read_segment(path: &Path) -> Result<Vec<ReplayedRecord>, StoreError> {
    decode_segment(path, &fs::read(path)?)
}

#[cfg(test)]
mod tests;
