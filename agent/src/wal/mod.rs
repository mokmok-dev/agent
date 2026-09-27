//! WORM write-ahead log for the event bus.
//!
//! Implements the physical record frame, its `crc32c` checksum, and the BLAKE3
//! hash chain described in `docs/event-bus/wal.md`. Segments, the group-commit
//! write path, the sparse index, and cursors are later milestones: this module
//! covers the part the design tests "without any network code".
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
mod scan;

#[cfg(kani)]
mod proofs;
#[cfg(test)]
mod tests;

pub use chain::{HASH_LEN, genesis_hash, hash_record};
pub use crc::crc32c;
pub use error::Error;
pub use frame::{FRAME_VERSION, Header, MAGIC, RECORD_OVERHEAD, Record, record_len};
pub use scan::{Entry, Recovery, recover, recover_from};
