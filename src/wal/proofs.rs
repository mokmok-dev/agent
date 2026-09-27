//! Kani harnesses for the WAL record frame.
//!
//! These cover the pure, index-heavy part of the format: length arithmetic,
//! framing, bounds, and chain validation. The BLAKE3 chain hash is not exercised
//! here: the `blake3` crate reaches `cpuid` inline assembly for runtime CPU
//! feature detection, which Kani cannot model. Hash continuity is covered by the
//! proptest reference model in `tests/wal.rs` instead.
//!
//! Run with `cargo kani --lib`. Because a chain-validation harness is handed a
//! caller-supplied starting hash, it never calls `hash_record` and so never
//! reaches `blake3`.

use super::chain::HASH_LEN;
use super::error::Error;
use super::frame::{FRAME_VERSION, HEADER_LEN, MAGIC, RECORD_OVERHEAD, Record, record_len};
use super::scan::recover_from;

/// Upper bound on symbolic payload lengths, so proof search stays finite.
const MAX_PAYLOAD: usize = 64;

/// A fixed chain head for the scan harnesses. It is a plain constant rather
/// than [`super::chain::genesis_hash`] so the proof does not pull in `blake3`.
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
fn record_len_is_exact() {
    let payload_len: usize = kani::any();
    kani::assume(payload_len <= MAX_PAYLOAD);
    assert_eq!(record_len(payload_len), Some(payload_len + RECORD_OVERHEAD));
}

#[kani::proof]
fn record_len_rejects_overflow() {
    assert_eq!(record_len(usize::MAX), None);
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
#[kani::unwind(80)]
fn encode_then_decode_round_trips() {
    let seq: u64 = kani::any();
    let timestamp_ns: u64 = kani::any();
    let payload = [kani::any::<u8>(), kani::any(), kani::any(), kani::any()];
    let prev_hash = [kani::any::<u8>(); HASH_LEN];

    let record = Record::new(seq, timestamp_ns, payload.to_vec(), prev_hash);
    let bytes = record
        .encode()
        .expect("a four-byte payload always fits the length field");
    assert_eq!(bytes.len(), record.total_len());

    let decoded = Record::decode(&bytes).expect("an encoded record always decodes");
    assert_eq!(decoded.header(), record.header());
    assert_eq!(decoded.payload(), &payload);
    assert_eq!(decoded.prev_hash(), &prev_hash);
}

#[kani::proof]
fn a_single_record_recovers_cleanly() {
    // The starting hash is passed in, so the scan never hashes and never reaches
    // `blake3`'s unsupported inline assembly.
    let bytes = single_record(START);
    let recovery = recover_from(&bytes, START).expect("a well-formed record recovers");

    assert!(recovery.is_clean());
    assert_eq!(recovery.committed_len(), bytes.len());
    assert_eq!(recovery.records().len(), 1);
    assert_eq!(recovery.head_seq(), Some(7));
}

#[kani::proof]
fn a_broken_chain_is_fatal() {
    let mut wrong = START;
    wrong[0] ^= 1;
    let bytes = single_record(wrong);

    assert!(matches!(
        recover_from(&bytes, START),
        Err(Error::ChainMismatch { .. })
    ));
}
