//! The fixed physical record frame.
//!
//! Layout (little-endian), per `docs/event-bus/wal.md`:
//!
//! ```text
//! offset  size  field
//! 0       4     magic            "AEB1"
//! 4       1     frame_version    1
//! 5       3     reserved         zero
//! 8       8     seq              u64
//! 16      8     timestamp_ns     u64
//! 24      4     payload_len      u32
//! 28      N     payload          bytes
//! 28+N    4     crc32c           over bytes [0, 28+N)
//! 32+N    32    prev_hash        BLAKE3-256 of the previous record
//! ```
//!
//! The frame is self-delimiting: a reader computes the end of a record from
//! `payload_len` without parsing the payload.

use crate::wal::chain::HASH_LEN;
use crate::wal::crc::crc32c;
use crate::wal::error::Error;

/// Record magic.
pub const MAGIC: [u8; 4] = *b"AEB1";

/// The frame version this build writes and reads.
pub const FRAME_VERSION: u8 = 1;

/// Fixed header length in bytes (everything before the payload).
pub const HEADER_LEN: usize = 28;

/// Bytes of per-record overhead: the header plus the checksum and `prev_hash`.
pub const RECORD_OVERHEAD: usize = HEADER_LEN + 4 + HASH_LEN;

const VERSION_OFFSET: usize = 4;
const RESERVED_OFFSET: usize = 5;
const SEQ_OFFSET: usize = 8;
const TIMESTAMP_OFFSET: usize = 16;
const PAYLOAD_LEN_OFFSET: usize = 24;

const CRC_WIDTH: usize = 4;
const RESERVED_LEN: usize = 3;

/// The fixed fields of a record, independent of the payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    /// Monotonically increasing sequence number.
    pub seq: u64,
    /// Bus commit time in nanoseconds since the Unix epoch.
    pub timestamp_ns: u64,
    /// Length of the payload in bytes.
    pub payload_len: u32,
}

/// A complete WAL record: its header, payload, and the link to its predecessor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    header: Header,
    payload: Vec<u8>,
    prev_hash: [u8; HASH_LEN],
}

/// Total size of a record with a `payload_len`-byte payload.
///
/// Returns `None` if the size cannot be represented, which a caller must treat
/// as [`Error::PayloadTooLong`] rather than wrap around.
#[must_use]
pub const fn record_len(payload_len: usize) -> Option<usize> {
    payload_len.checked_add(RECORD_OVERHEAD)
}

impl Record {
    /// Assemble a record.
    #[must_use]
    pub fn new(
        seq: u64,
        timestamp_ns: u64,
        payload: Vec<u8>,
        prev_hash: [u8; HASH_LEN],
    ) -> Self {
        let header = Header {
            seq,
            timestamp_ns,
            payload_len: u32::try_from(payload.len()).unwrap_or(u32::MAX),
        };
        Self {
            header,
            payload,
            prev_hash,
        }
    }

    /// The record's fixed header.
    #[must_use]
    pub const fn header(&self) -> Header {
        self.header
    }

    /// The record's sequence number.
    #[must_use]
    pub const fn seq(&self) -> u64 {
        self.header.seq
    }

    /// The record's commit timestamp.
    #[must_use]
    pub const fn timestamp_ns(&self) -> u64 {
        self.header.timestamp_ns
    }

    /// The record's payload bytes.
    #[must_use]
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }

    /// The link to the previous record's hash.
    #[must_use]
    pub const fn prev_hash(&self) -> &[u8; HASH_LEN] {
        &self.prev_hash
    }

    /// Total encoded size of this record.
    #[must_use]
    pub const fn total_len(&self) -> usize {
        self.header.payload_len as usize + RECORD_OVERHEAD
    }

    /// Serialize the record, computing its `crc32c`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::PayloadTooLong`] if the payload does not fit the 32-bit
    /// length field.
    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        let payload_len = u32::try_from(self.payload.len())
            .map_err(|_| Error::PayloadTooLong(self.payload.len()))?;

        let mut bytes = Vec::with_capacity(self.payload.len() + RECORD_OVERHEAD);
        bytes.extend_from_slice(&MAGIC);
        bytes.push(FRAME_VERSION);
        bytes.extend_from_slice(&[0u8; RESERVED_LEN]);
        bytes.extend_from_slice(&self.header.seq.to_le_bytes());
        bytes.extend_from_slice(&self.header.timestamp_ns.to_le_bytes());
        bytes.extend_from_slice(&payload_len.to_le_bytes());
        bytes.extend_from_slice(&self.payload);
        let checksum = crc32c(&bytes);
        bytes.extend_from_slice(&checksum.to_le_bytes());
        bytes.extend_from_slice(&self.prev_hash);
        Ok(bytes)
    }

    /// Parse one record from the front of `bytes`.
    ///
    /// Validates the magic, version, reserved bytes, framing, and `crc32c`. It
    /// does not check the hash chain; [`crate::wal::recover`] does that across
    /// records. Bytes after the record are ignored, so a caller can advance by
    /// [`Record::total_len`].
    ///
    /// # Errors
    ///
    /// Returns a [`Error`] describing the first structural or checksum
    /// failure.
    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let header = parse_header(bytes)?;
        let payload_start = HEADER_LEN;
        let payload_end = payload_start + header.payload_len as usize;
        let crc_end = payload_end + CRC_WIDTH;
        let record_end = crc_end + HASH_LEN;
        if bytes.len() < record_end {
            return Err(Error::TruncatedPayload {
                payload_len: header.payload_len,
                remaining: bytes.len().saturating_sub(payload_start),
            });
        }

        let stored = u32::from_le_bytes([
            bytes[payload_end],
            bytes[payload_end + 1],
            bytes[payload_end + 2],
            bytes[payload_end + 3],
        ]);
        let computed = crc32c(&bytes[..payload_end]);
        if stored != computed {
            return Err(Error::CrcMismatch { stored, computed });
        }

        let mut prev_hash = [0u8; HASH_LEN];
        prev_hash.copy_from_slice(&bytes[crc_end..record_end]);

        Ok(Self {
            header,
            payload: bytes[payload_start..payload_end].to_vec(),
            prev_hash,
        })
    }
}

/// Read a fixed-width little-endian field at `offset`.
///
/// The caller has already checked that `offset + N` is within the buffer, so
/// the slicing cannot panic.
fn read_array<const N: usize>(
    bytes: &[u8],
    offset: usize,
) -> [u8; N] {
    let mut field = [0u8; N];
    field.copy_from_slice(&bytes[offset..offset + N]);
    field
}

/// Parse and validate the fixed header at the front of `bytes`.
pub fn parse_header(bytes: &[u8]) -> Result<Header, Error> {
    if bytes.len() < HEADER_LEN {
        return Err(Error::TruncatedHeader(bytes.len()));
    }
    if bytes[..MAGIC.len()] != MAGIC {
        return Err(Error::BadMagic);
    }
    let version = bytes[VERSION_OFFSET];
    if version != FRAME_VERSION {
        return Err(Error::UnsupportedVersion(version));
    }
    if bytes[RESERVED_OFFSET..RESERVED_OFFSET + RESERVED_LEN] != [0u8; RESERVED_LEN] {
        return Err(Error::ReservedNotZero);
    }

    let seq = u64::from_le_bytes(read_array(bytes, SEQ_OFFSET));
    let timestamp_ns = u64::from_le_bytes(read_array(bytes, TIMESTAMP_OFFSET));
    let payload_len = u32::from_le_bytes(read_array(bytes, PAYLOAD_LEN_OFFSET));

    Ok(Header {
        seq,
        timestamp_ns,
        payload_len,
    })
}
