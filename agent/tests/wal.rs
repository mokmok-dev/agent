//! Property tests for the WAL. The reference model is an in-memory list of the
//! records that were written; recovery must reproduce it, and any single-byte
//! mutation must be detected.

use agent::wal::{Error, HASH_LEN, Record, genesis_hash, hash_record, recover};
use proptest::prelude::*;

/// One record's inputs: sequence, timestamp, and payload.
type Spec = (u64, u64, Vec<u8>);

/// Serialize `spec` into a log, chaining each record to the hash of the
/// previous one and returning the bytes plus each record's byte range.
#[expect(
    clippy::expect_used,
    reason = "payloads are bounded to 64 bytes, so encoding cannot fail"
)]
fn write_log(spec: &[Spec]) -> (Vec<u8>, Vec<(usize, usize)>) {
    let mut log = Vec::new();
    let mut ranges = Vec::new();
    let mut prev = genesis_hash();

    for (seq, timestamp_ns, payload) in spec {
        let record = Record::new(*seq, *timestamp_ns, payload.clone(), prev);
        let bytes = record
            .encode()
            .expect("payloads are small enough to encode");
        let start = log.len();
        let end = start + bytes.len();
        ranges.push((start, end));
        prev = hash_record(&bytes);
        log.extend_from_slice(&bytes);
    }

    (log, ranges)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn recovery_reproduces_the_written_log(
        spec in prop::collection::vec(
            (any::<u64>(), any::<u64>(), prop::collection::vec(any::<u8>(), 0..=64)),
            0..24,
        )
    ) {
        let (log, ranges) = write_log(&spec);
        let recovery = recover(&log).expect("a log we just wrote recovers");

        prop_assert!(recovery.is_clean());
        prop_assert_eq!(recovery.committed_len(), log.len());
        prop_assert_eq!(recovery.records().len(), spec.len());
        prop_assert_eq!(recovery.head_seq(), spec.last().map(|(seq, _, _)| *seq));

        for (index, (entry, (seq, timestamp_ns, payload))) in
            recovery.records().iter().zip(&spec).enumerate()
        {
            prop_assert_eq!(entry.seq(), *seq);
            let (start, end) = ranges[index];
            prop_assert_eq!(entry.offset(), start);
            prop_assert_eq!(entry.byte_len(), end - start);

            let decoded = Record::decode(&log[start..end]).unwrap();
            prop_assert_eq!(decoded.seq(), *seq);
            prop_assert_eq!(decoded.timestamp_ns(), *timestamp_ns);
            prop_assert_eq!(decoded.payload(), payload.as_slice());
            prop_assert_eq!(decoded.total_len(), end - start);
        }
    }

    #[test]
    fn a_torn_tail_is_truncated(
        spec in prop::collection::vec(
            (any::<u64>(), any::<u64>(), prop::collection::vec(any::<u8>(), 0..=64)),
            1..12,
        ),
        tail_len in 0usize..96,
    ) {
        let (log, _) = write_log(&spec);
        let committed = log.len();

        // Append a prefix of a well-formed next record, as a crash mid-write
        // would leave behind.
        let next = Record::new(999, 0, vec![1, 2, 3, 4], genesis_hash())
            .encode()
            .expect("a four-byte payload always fits the length field");
        let mut torn = log;
        // `min(next.len() - 1)` keeps the appended bytes a strict prefix of a
        // well-formed record, which is what a crash mid-write leaves behind.
        torn.extend_from_slice(&next[..tail_len.min(next.len() - 1)]);

        let recovery = recover(&torn).expect("a torn tail is not a fatal corruption");

        // Only whole records before the tail are committed, and a torn tail
        // must reduce `is_clean`.
        prop_assert_eq!(recovery.committed_len(), committed);
        prop_assert_eq!(recovery.total_len(), torn.len());
        prop_assert_eq!(recovery.is_clean(), tail_len == 0);
        prop_assert_eq!(recovery.head_seq(), spec.last().map(|(seq, _, _)| *seq));
    }

    #[test]
    fn a_payload_flip_is_detected(
        spec in prop::collection::vec(
            (any::<u64>(), any::<u64>(), prop::collection::vec(any::<u8>(), 1..=64)),
            1..12,
        ),
        record_index in 0usize..12,
        byte_index in 0usize..64,
        xor in 1u8..=255,
    ) {
        let (log, ranges) = write_log(&spec);
        let index = record_index % spec.len();
        let (start, end) = ranges[index];
        let payload = spec[index].2.as_slice();

        // Mutate one payload byte. `payload` is `1..=64`, so the computed
        // offset stays inside the record regardless of the two random indices.
        let offset_in_payload = byte_index % payload.len();
        let absolute = start + 28 + offset_in_payload;
        prop_assert!(absolute < end);

        let mut mutated = log;
        mutated[absolute] ^= xor;

        let detected = matches!(recover(&mutated), Err(Error::CrcMismatch { .. }));
        prop_assert!(detected, "a payload flip must be caught by crc32c");
    }

    #[test]
    fn a_prev_hash_flip_breaks_the_chain(
        spec in prop::collection::vec(
            (any::<u64>(), any::<u64>(), prop::collection::vec(any::<u8>(), 0..=64)),
            1..12,
        ),
        record_index in 0usize..12,
        hash_index in 0usize..HASH_LEN,
        xor in 1u8..=255,
    ) {
        let (log, ranges) = write_log(&spec);
        let index = record_index % spec.len();
        let (start, end) = ranges[index];

        // `prev_hash` is the trailing 32 bytes of a record and is deliberately
        // outside the crc-covered region, so only the chain catches it.
        let absolute = end - HASH_LEN + hash_index;
        prop_assert!(absolute >= start && absolute < end);

        let mut mutated = log;
        mutated[absolute] ^= xor;

        let detected = matches!(recover(&mutated), Err(Error::ChainMismatch { .. }));
        prop_assert!(detected, "a prev_hash flip must break the chain");
    }
}
