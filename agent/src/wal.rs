//! WORM write-ahead log for the event bus.
//!
//! Implements the physical record frame, its `crc32c` checksum, the BLAKE3 hash
//! chain, and the segmented file store described in `docs/event-bus/wal.md`.
//!
//! - [`Record`] is the physical frame.
//! - [`recover`] and [`recover_from`] validate a byte slice.
//! - [`Store`] is the file-backed, segmented writer and reader.
//! - [`verify_dir`] validates a whole store directory.
//!
//! The sparse index and cursors are later milestones.
//!
//! ```
//! use agent::wal::{Record, genesis_hash};
//!
//! let record = Record::new(1, 0, b"hello".to_vec(), genesis_hash());
//! let bytes = record.encode()?;
//! let decoded = Record::decode(&bytes)?;
//!
//! assert_eq!(decoded.seq(), 1);
//! assert_eq!(decoded.payload(), b"hello");
//! assert_eq!(decoded.total_len(), bytes.len());
//! # Ok::<(), agent::wal::Error>(())
//! ```

mod chain;
mod crc;
mod error;
mod frame;
mod replay;
mod scan;
mod store;

#[cfg(kani)]
mod proofs;

pub use chain::{HASH_LEN, genesis_hash, hash_record};
pub use crc::crc32c;
pub use error::Error;
pub use frame::{FRAME_VERSION, Header, MAGIC, RECORD_OVERHEAD, Record, record_len};
pub use replay::{Replay, ReplayedRecord};
pub use scan::{Entry, Recovery, recover, recover_from};
pub use store::{Committed, DEFAULT_SEGMENT_SIZE, Store, StoreError, VerifyReport, verify_dir};

#[cfg(test)]
mod tests {
    // Focused unit tests for the frame's rejection and boundary paths, which the
    // reference-model proptest in `tests/wal.rs` does not reach directly.

    use super::frame::HEADER_LEN;
    use super::*;

    /// A record with a four-byte payload chained to `prev`.
    fn record(prev: [u8; HASH_LEN]) -> Vec<u8> {
        Record::new(0, 0, vec![1, 2, 3, 4], prev)
            .encode()
            .expect("a four-byte payload always encodes")
    }

    #[test]
    fn bad_magic_is_rejected() {
        let mut bytes = record(genesis_hash());
        bytes[0] = b'X';
        assert_eq!(Record::decode(&bytes), Err(Error::BadMagic));
    }

    #[test]
    fn an_unsupported_version_is_rejected() {
        let mut bytes = record(genesis_hash());
        bytes[4] = FRAME_VERSION.wrapping_add(1);
        assert_eq!(
            Record::decode(&bytes),
            Err(Error::UnsupportedVersion(FRAME_VERSION.wrapping_add(1)))
        );
    }

    #[test]
    fn a_nonzero_reserved_byte_is_rejected() {
        let mut bytes = record(genesis_hash());
        bytes[5] = 1;
        assert_eq!(Record::decode(&bytes), Err(Error::ReservedNotZero));
    }

    #[test]
    fn a_buffer_shorter_than_the_header_is_truncated() {
        let bytes = record(genesis_hash());
        let short = &bytes[..HEADER_LEN - 1];
        assert_eq!(
            Record::decode(short),
            Err(Error::TruncatedHeader(HEADER_LEN - 1))
        );
    }

    #[test]
    fn a_record_missing_its_trailer_is_truncated() {
        let bytes = record(genesis_hash());
        // Drop the last byte: the header and payload are whole, but the checksum and
        // `prev_hash` are not, so the reader must not index past the buffer.
        let short = &bytes[..bytes.len() - 1];
        assert!(matches!(
            Record::decode(short),
            Err(Error::TruncatedPayload { .. })
        ));
    }

    #[test]
    fn a_header_claiming_more_payload_than_present_is_torn() {
        let mut bytes = record(genesis_hash());
        // Enlarge the declared payload so the record claims more bytes than exist.
        bytes[24..28].copy_from_slice(&20u32.to_le_bytes());

        let recovery = recover(&bytes).expect("a torn write is not a fatal corruption");
        assert_eq!(recovery.committed_len(), 0);
        assert!(!recovery.is_clean());
    }

    #[test]
    fn a_whole_record_with_a_bad_checksum_is_fatal() {
        let mut bytes = record(genesis_hash());
        bytes[10] ^= 0xFF; // a `seq` byte, inside the crc-covered region
        assert!(matches!(recover(&bytes), Err(Error::CrcMismatch { .. })));
    }

    #[test]
    fn a_full_header_of_garbage_at_the_tail_is_fatal() {
        let mut bytes = record(genesis_hash());
        // Exactly a header's worth of bytes with a bad magic. The header is present,
        // so this is corruption and not a torn trailing write.
        bytes.extend_from_slice(&[0u8; HEADER_LEN]);
        assert_eq!(recover(&bytes), Err(Error::BadMagic));
    }
}
