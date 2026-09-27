//! BLAKE3 hash chain over WAL records.

/// Length of a BLAKE3-256 digest in bytes.
pub const HASH_LEN: usize = 32;

/// Domain separation string for the genesis link `h_0`.
///
/// The genesis hash is the `prev_hash` of the first record, so a log cannot be
/// silently replaced by a different log that happens to start with `prev_hash
/// = 0`.
const GENESIS_DOMAIN: &[u8] = b"agent-eventbus-wal-genesis-v1";

/// The genesis hash `h_0`.
#[must_use]
pub fn genesis_hash() -> [u8; HASH_LEN] {
    *blake3::hash(GENESIS_DOMAIN).as_bytes()
}

/// The chain hash `h_n` of a complete record's bytes.
///
/// The caller passes the record including its trailing `prev_hash`, so the
/// hash commits to the whole frame.
#[must_use]
pub fn hash_record(bytes: &[u8]) -> [u8; HASH_LEN] {
    *blake3::hash(bytes).as_bytes()
}
