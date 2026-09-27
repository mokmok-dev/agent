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
#[cfg(test)]
mod tests;

pub use chain::{HASH_LEN, genesis_hash, hash_record};
pub use crc::crc32c;
pub use error::Error;
pub use frame::{FRAME_VERSION, Header, MAGIC, RECORD_OVERHEAD, Record, record_len};
pub use replay::{Replay, ReplayedRecord};
pub use scan::{Entry, Recovery, recover, recover_from};
pub use store::{Committed, DEFAULT_SEGMENT_SIZE, Store, StoreError, VerifyReport, verify_dir};
