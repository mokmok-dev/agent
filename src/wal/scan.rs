//! Log scan, verification, and crash recovery.
//!
//! [`recover`] is a linear pass from the first record. It validates each
//! record's `crc32c`, walks the BLAKE3 chain, and truncates only a torn
//! trailing record. Per `docs/event-bus/wal.md`, a checksum or chain failure in
//! any other record is fatal: the caller must refuse to trust the log rather
//! than start from it.

use crate::wal::chain::{HASH_LEN, genesis_hash, hash_record};
use crate::wal::error::Error;
use crate::wal::frame::{HEADER_LEN, Record, parse_header, record_len};

/// One validated record located in the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    seq: u64,
    offset: usize,
    len: usize,
}

impl Entry {
    /// The record's sequence number.
    #[must_use]
    pub const fn seq(self) -> u64 {
        self.seq
    }

    /// Byte offset of the record from the start of the log.
    #[must_use]
    pub const fn offset(self) -> usize {
        self.offset
    }

    /// Total encoded length of the record in bytes.
    #[must_use]
    pub const fn byte_len(self) -> usize {
        self.len
    }
}

/// The result of scanning a log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recovery {
    records: Vec<Entry>,
    committed_len: usize,
    total_len: usize,
}

impl Recovery {
    /// The validated records, in sequence order.
    #[must_use]
    pub fn records(&self) -> &[Entry] {
        &self.records
    }

    /// Bytes of the log that are covered by validated records.
    ///
    /// Bytes beyond this are a torn trailing record that recovery truncates.
    #[must_use]
    pub const fn committed_len(&self) -> usize {
        self.committed_len
    }

    /// The total length of the scanned input.
    #[must_use]
    pub const fn total_len(&self) -> usize {
        self.total_len
    }

    /// Whether the whole input consisted of validated records.
    #[must_use]
    pub const fn is_clean(&self) -> bool {
        self.committed_len == self.total_len
    }

    /// The sequence number of the last validated record, if any.
    #[must_use]
    pub fn head_seq(&self) -> Option<u64> {
        self.records.last().map(|entry| entry.seq)
    }
}

/// Scan `bytes` end to end, validating every record.
///
/// A torn trailing record (a prefix of a valid record at the end of the input)
/// is truncated and reported via [`Recovery::committed_len`]. Any other
/// structural, checksum, or chain failure is returned as an [`Error`].
///
/// # Errors
///
/// Returns the first [`Error`] that is not a torn trailing record.
pub fn recover(bytes: &[u8]) -> Result<Recovery, Error> {
    recover_from(bytes, genesis_hash())
}

/// Scan `bytes`, validating the chain against a caller-supplied starting hash.
///
/// [`recover`] passes [`genesis_hash`]. A scan that starts mid-log, such as the
/// first record of a segment, passes the hash of the last record of the
/// preceding segment so the chain is validated across the boundary.
///
/// # Errors
///
/// Returns the first [`Error`] that is not a torn trailing record.
pub fn recover_from(
    bytes: &[u8],
    start_hash: [u8; HASH_LEN],
) -> Result<Recovery, Error> {
    let mut position = 0usize;
    // The range of the record validated last, whose hash is the expected link
    // for the next record. `hash_record` is only called once a second record
    // exists, so a single-record scan does no hashing.
    let mut previous: Option<(usize, usize)> = None;
    let mut records = Vec::new();

    while position < bytes.len() {
        // A partial fixed header can only be a torn trailing write.
        if bytes.len() - position < HEADER_LEN {
            break;
        }
        // The header is fully present, so a bad magic/version/reserved byte is
        // corruption, not a torn write.
        let header = parse_header(&bytes[position..])?;

        let Some(len) = record_len(header.payload_len as usize) else {
            break;
        };
        if len > bytes.len() - position {
            // The declared payload runs past the end of the log: torn write.
            break;
        }

        let end = position + len;
        // A full-length record with a bad checksum cannot be safely truncated,
        // so `Record::decode`'s error is fatal.
        let record = Record::decode(&bytes[position..end])?;
        let expected = match previous {
            Some((start, stop)) => hash_record(&bytes[start..stop]),
            None => start_hash,
        };
        if record.prev_hash() != &expected {
            return Err(Error::ChainMismatch { seq: header.seq });
        }

        records.push(Entry {
            seq: header.seq,
            offset: position,
            len,
        });
        previous = Some((position, end));
        position = end;
    }

    Ok(Recovery {
        records,
        committed_len: position,
        total_len: bytes.len(),
    })
}
