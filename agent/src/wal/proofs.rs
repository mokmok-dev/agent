//! Kani harnesses for the WAL record frame.
//!
//! These are deliberately few. Kani's unique value here is exhaustive bounds
//! safety over attacker-controlled bytes: the proptest samples lengths, while
//! these prove that *no* input reaches an out-of-bounds read. Properties that a
//! reference model already covers over many inputs — encode/decode round-trip,
//! chain-link detection, and plain length arithmetic — are left to
//! `tests/wal.rs` and `tests.rs` rather than restated as expensive proofs.
//!
//! Run with `cargo kani -p agent --lib`. Because a scan harness is handed a
//! caller-supplied starting hash, it never calls `hash_record`, so the proofs
//! never reach `blake3`'s `cpuid` inline assembly.

use super::chain::HASH_LEN;
use super::error::Error;
use super::frame::{FRAME_VERSION, HEADER_LEN, MAGIC, Record};
use super::scan::recover_from;

/// Upper bound on symbolic payload lengths, so proof search stays finite.
const MAX_PAYLOAD: usize = 64;

/// A fixed chain head for the scan harness. It is a plain constant rather than
/// [`super::chain::genesis_hash`] so the proof does not pull in `blake3`.
const START: [u8; HASH_LEN] = [0u8; HASH_LEN];

/// A log consisting of exactly one record with a four-byte payload.
fn single_record(prev_hash: [u8; HASH_LEN]) -> Vec<u8> {
    Record::new(7, 99, vec![0, 1, 2, 3], prev_hash)
        .encode()
        .expect("a four-byte payload always fits the length field")
}

/// A valid fixed header with `payload_len` bytes declared.
fn header_with(payload_len: u32) -> [u8; HEADER_LEN] {
    let mut buf = [0u8; HEADER_LEN];
    buf[..4].copy_from_slice(&MAGIC);
    buf[4] = FRAME_VERSION;
    buf[24..28].copy_from_slice(&payload_len.to_le_bytes());
    buf
}

#[kani::proof]
fn a_truncated_payload_past_the_header_is_reported() {
    let payload_len: usize = kani::any();
    kani::assume(payload_len <= MAX_PAYLOAD);
    let buf = header_with(payload_len as u32);

    // The header is present but none of the declared payload follows, so the
    // reader must report a truncation instead of reading past the buffer.
    assert!(matches!(
        Record::decode(&buf),
        Err(Error::TruncatedPayload { .. })
    ));
}

#[kani::proof]
fn a_short_header_is_rejected_without_reading_past_it() {
    let full: [u8; HEADER_LEN] = kani::any();
    let len: usize = kani::any();
    kani::assume(len < HEADER_LEN);
    assert_eq!(
        Record::decode(&full[..len]),
        Err(Error::TruncatedHeader(len))
    );
}

#[kani::proof]
fn a_single_record_recovers_cleanly() {
    // The starting hash is passed in, so the scan never hashes.
    let bytes = single_record(START);
    let recovery = recover_from(&bytes, START).expect("a well-formed record recovers");

    assert!(recovery.is_clean());
    assert_eq!(recovery.committed_len(), bytes.len());
    assert_eq!(recovery.records().len(), 1);
    assert_eq!(recovery.head_seq(), Some(7));
}
