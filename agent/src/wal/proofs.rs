//! Kani harnesses for the WAL record frame.
//!
//! These are deliberately few. Kani's unique value here is exhaustive bounds
//! safety over attacker-controlled bytes: the proptest samples lengths, while
//! these prove that *no* input reaches an out-of-bounds read. Properties that a
//! reference model already covers over many inputs — encode/decode round-trip,
//! chain-link detection, and plain length arithmetic — are left to
//! `tests/wal.rs` and `tests.rs` rather than restated as expensive proofs.
//!
//! Run with `cargo kani -p agent --lib`. These harnesses do not hash, so the
//! proofs never reach `blake3`'s `cpuid` inline assembly, and they stay off the
//! scan/recovery path, whose CBMC encoding is far more expensive.

use super::error::Error;
use super::frame::{FRAME_VERSION, HEADER_LEN, MAGIC, Record};

/// Upper bound on symbolic payload lengths, so proof search stays finite.
const MAX_PAYLOAD: usize = 64;

/// A valid fixed header with `payload_len` bytes declared.
fn header_with(payload_len: u32) -> [u8; HEADER_LEN] {
    let mut buf = [0u8; HEADER_LEN];
    buf[..4].copy_from_slice(&MAGIC);
    buf[4] = FRAME_VERSION;
    buf[24..28].copy_from_slice(&payload_len.to_le_bytes());
    buf
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
