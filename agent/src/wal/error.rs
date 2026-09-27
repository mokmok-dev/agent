//! Failures of the WAL frame reader and writer.

use crate::wal::frame::HEADER_LEN;

/// A structural, checksum, or chain failure in a WAL record.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    /// The buffer ended before the fixed header.
    #[error("record is shorter than the {HEADER_LEN}-byte header: {0} bytes")]
    TruncatedHeader(usize),
    /// The declared payload does not fit the remaining buffer.
    #[error(
        "record declares a {payload_len}-byte payload but only {remaining} bytes follow the header"
    )]
    TruncatedPayload {
        /// The length the header declares.
        payload_len: u32,
        /// Bytes available after the header.
        remaining: usize,
    },
    /// The first four bytes are not the record magic.
    #[error("record magic is not AEB1")]
    BadMagic,
    /// The frame version is not the one this build understands.
    #[error("unsupported frame version {0}")]
    UnsupportedVersion(u8),
    /// A reserved header byte is not zero.
    #[error("reserved header bytes are not zero")]
    ReservedNotZero,
    /// The stored checksum does not match the computed one.
    #[error("crc32c mismatch: stored {stored:#010x}, computed {computed:#010x}")]
    CrcMismatch {
        /// The checksum stored in the record.
        stored: u32,
        /// The checksum computed over the record's bytes.
        computed: u32,
    },
    /// A record's `prev_hash` does not match the previous record's hash.
    #[error("hash chain is broken at seq {seq}")]
    ChainMismatch {
        /// The sequence number of the record whose link failed.
        seq: u64,
    },
    /// The payload does not fit the 32-bit length field.
    #[error("payload of {0} bytes exceeds the u32 length field")]
    PayloadTooLong(usize),
}
