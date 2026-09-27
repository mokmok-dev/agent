//! Replay of committed records from the log.
//!
//! [`Replay`] reads records back from a starting sequence number, one segment at
//! a time, so replaying a long history holds at most one segment in memory
//! rather than the whole log. The store already validated framing and the hash
//! chain when it was opened (see [`crate::wal::Store::open`]), so replay decodes
//! records without re-walking the chain; each record's `crc32c` is still
//! checked.
//!
//! Replay is byte-oriented: it yields a record's payload, and the caller decodes
//! the `CloudEvents` envelope. This keeps the WAL independent of the event
//! model.

use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};

use crate::wal::frame::{HEADER_LEN, Record, parse_header, record_len};
use crate::wal::store::{StoreError, segment_files};

/// A committed record read back from the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayedRecord {
    /// The record's sequence number.
    pub seq: u64,
    /// The commit timestamp in nanoseconds since the Unix epoch.
    pub timestamp_ns: u64,
    /// The record's payload bytes.
    pub payload: Vec<u8>,
}

/// A lazy reader over committed records from a start sequence.
///
/// Produced by [`crate::wal::Store::replay`]. Each [`Iterator::next`] yields one
/// record in sequence order, or a [`StoreError`] if a segment cannot be read.
/// After an error, the iterator ends.
#[derive(Debug)]
pub struct Replay {
    segments: VecDeque<PathBuf>,
    current: VecDeque<ReplayedRecord>,
    from_seq: u64,
    finished: bool,
}

impl Replay {
    /// Build a replay over `dir` starting at `from_seq`.
    ///
    /// # Errors
    ///
    /// Returns a [`StoreError`] if the directory cannot be listed.
    pub(crate) fn new(
        dir: &Path,
        from_seq: u64,
    ) -> Result<Self, StoreError> {
        let mut files = segment_files(dir)?;
        files.sort_by_key(|(first_seq, _)| *first_seq);

        // Start at the segment that may contain `from_seq`: the last segment
        // whose first sequence is at or below it, or the first segment when
        // `from_seq` precedes all of them. An empty store yields nothing.
        let start = match files
            .iter()
            .position(|(first_seq, _)| *first_seq > from_seq)
        {
            Some(0) => 0,
            Some(index) => index - 1,
            None => files.len().saturating_sub(1),
        };

        Ok(Self {
            segments: files
                .into_iter()
                .skip(start)
                .map(|(_, path)| path)
                .collect(),
            current: VecDeque::new(),
            from_seq,
            finished: false,
        })
    }
}

impl Iterator for Replay {
    type Item = Result<ReplayedRecord, StoreError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(record) = self.current.pop_front() {
                return Some(Ok(record));
            }
            if self.finished {
                return None;
            }
            let Some(path) = self.segments.pop_front() else {
                self.finished = true;
                return None;
            };
            match read_segment(&path) {
                Ok(records) => {
                    // The segment that contains `from_seq` also holds earlier
                    // records, which must not be delivered.
                    let from_seq = self.from_seq;
                    self.current = records
                        .into_iter()
                        .filter(|record| record.seq >= from_seq)
                        .collect();
                },
                Err(error) => {
                    self.finished = true;
                    return Some(Err(error));
                },
            }
        }
    }
}

/// Decode every whole record in a segment's bytes.
///
/// The store validated the segment's chain at open, so this checks only framing
/// and each record's `crc32c`. A torn trailing record — a prefix of a record at
/// the end — is not an error: it ends the scan, matching recovery's rule that
/// only the last segment may end in a partial write. The store truncates that
/// tail on open, so this is defensive.
fn decode_segment(
    path: &Path,
    mut bytes: &[u8],
) -> Result<Vec<ReplayedRecord>, StoreError> {
    let mut records = Vec::new();

    while !bytes.is_empty() {
        // A partial fixed header can only be a torn trailing write.
        if bytes.len() < HEADER_LEN {
            break;
        }
        let header = parse_header(bytes).map_err(|source| StoreError::Segment {
            path: path.to_path_buf(),
            source,
        })?;
        let Some(len) = record_len(header.payload_len as usize) else {
            break;
        };
        if bytes.len() < len {
            break;
        }
        let (record_bytes, rest) = bytes.split_at(len);
        let record = Record::decode(record_bytes).map_err(|source| StoreError::Segment {
            path: path.to_path_buf(),
            source,
        })?;
        records.push(ReplayedRecord {
            seq: record.seq(),
            timestamp_ns: record.timestamp_ns(),
            payload: record.payload().to_vec(),
        });
        bytes = rest;
    }

    Ok(records)
}

/// Read and decode a segment file.
fn read_segment(path: &Path) -> Result<Vec<ReplayedRecord>, StoreError> {
    decode_segment(path, &fs::read(path)?)
}

#[cfg(test)]
mod tests {
    // Tests for replay: the start sequence, segment-spanning iteration, and the
    // error an unreadable segment produces.

    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::decode_segment;
    use crate::wal::{Record, Store, StoreError, genesis_hash, hash_record};

    /// A placeholder path for `decode_segment`, whose error carries it back.
    fn dummy_path() -> &'static Path {
        Path::new("segment-test.log")
    }

    /// A unique, empty store directory removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "agent-replay-{tag}-{}-{unique}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&dir);
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Write `payloads` into a fresh store, one record each.
    fn store_with(
        dir: &TempDir,
        payloads: &[&[u8]],
    ) -> Store {
        let mut store = Store::open(&dir.0).expect("opens");
        for payload in payloads {
            store.append(payload).expect("appends");
        }
        store
    }

    /// Collect the sequence numbers a replay yields.
    fn sequences(
        store: &Store,
        from_seq: u64,
    ) -> Vec<u64> {
        store
            .replay(from_seq)
            .expect("lists segments")
            .map(|record| record.expect("reads").seq)
            .collect()
    }

    #[test]
    fn replaying_an_empty_store_yields_nothing() {
        let dir = TempDir::new("empty");
        let store = Store::open(&dir.0).expect("opens");
        assert!(sequences(&store, 0).is_empty());
    }

    #[test]
    fn replay_from_zero_yields_every_record_in_order() {
        let dir = TempDir::new("from-zero");
        let store = store_with(&dir, &[b"a", b"b", b"c"]);
        assert_eq!(sequences(&store, 0), [0, 1, 2]);
    }

    #[test]
    fn replay_skips_records_before_the_start() {
        let dir = TempDir::new("skip");
        let store = store_with(&dir, &[b"a", b"b", b"c", b"d"]);
        assert_eq!(sequences(&store, 2), [2, 3]);
    }

    #[test]
    fn replay_from_beyond_the_head_yields_nothing() {
        let dir = TempDir::new("beyond");
        let store = store_with(&dir, &[b"a", b"b"]);
        assert!(sequences(&store, 99).is_empty());
    }

    #[test]
    fn replayed_payloads_and_timestamps_match_what_was_written() {
        let dir = TempDir::new("payloads");
        let store = store_with(&dir, &[b"one", b"two", b"three"]);

        let records: Vec<_> = store
            .replay(1)
            .expect("lists")
            .map(|record| record.expect("reads"))
            .collect();

        assert_eq!(records.len(), 2);
        assert_eq!(records[0].seq, 1);
        assert_eq!(records[0].payload, b"two");
        assert_eq!(records[1].payload, b"three");
    }

    #[test]
    fn replay_spans_segment_boundaries() {
        let dir = TempDir::new("segments");
        // A tiny target forces many segments; replay must walk them in order.
        let record_len = 5 + crate::wal::RECORD_OVERHEAD;
        let mut store = Store::with_segment_size(&dir.0, record_len as u64 + 1).expect("opens");
        for _ in 0..20 {
            store.append(b"payload").expect("appends");
        }
        assert!(
            crate::wal::store::segment_files(&dir.0)
                .expect("lists")
                .len()
                > 1,
            "the small target must rotate",
        );

        let all = sequences(&store, 0);
        assert_eq!(all.len(), 20);
        // Every sequence appears exactly once, in ascending order.
        assert_eq!(all, (0..20).collect::<Vec<_>>());

        // A start near the end still finds its record across a boundary.
        assert_eq!(sequences(&store, 17), [17, 18, 19]);
    }

    #[test]
    fn replay_filters_within_the_starting_segment() {
        let dir = TempDir::new("filter");
        // One record per segment, so the starting segment holds exactly one record
        // and the filter must drop it when `from_seq` is higher.
        let record_len = 3 + crate::wal::RECORD_OVERHEAD;
        let mut store = Store::with_segment_size(&dir.0, record_len as u64 + 1).expect("opens");
        for _ in 0..6 {
            store.append(b"abc").expect("appends");
        }

        // `from_seq == 0` starts at the first segment.
        assert_eq!(sequences(&store, 0), [0, 1, 2, 3, 4, 5]);
        // A start inside a later segment must skip the earlier segments entirely.
        assert_eq!(sequences(&store, 4), [4, 5]);
    }

    #[test]
    fn a_corrupt_segment_is_reported_by_replay() {
        let dir = TempDir::new("corrupt");
        let store = store_with(&dir, &[b"a", b"b"]);

        // Replace a segment with a whole header's worth of garbage. The header is
        // present, so this is corruption rather than a torn tail.
        let path = crate::wal::store::segment_files(&dir.0)
            .expect("lists")
            .into_iter()
            .map(|(_, path)| path)
            .next()
            .expect("one segment");
        fs::write(&path, [0u8; 28]).expect("writes garbage");

        let error = store
            .replay(0)
            .expect("lists")
            .find_map(Result::err)
            .expect("the corrupt segment is reported");
        assert!(matches!(error, StoreError::Segment { .. }));
    }

    #[test]
    fn a_segment_deleted_after_open_is_simply_absent_from_replay() {
        let dir = TempDir::new("missing");
        let store = store_with(&dir, &[b"a", b"b"]);

        // Replay lists the segment files when it starts, so a segment removed before
        // then is not part of the replay rather than an error.
        for (_, path) in crate::wal::store::segment_files(&dir.0).expect("lists") {
            fs::remove_file(path).expect("removes the segment");
        }

        let result: Result<Vec<_>, StoreError> = store.replay(0).expect("lists").collect();
        assert!(matches!(result, Ok(records) if records.is_empty()));
    }

    /// Encode `count` chained records into one byte buffer.
    fn encode_records(count: u64) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut prev = genesis_hash();
        for seq in 0..count {
            let encoded = Record::new(seq, seq, vec![byte_of(seq)], prev)
                .encode()
                .expect("encodes");
            prev = hash_record(&encoded);
            bytes.extend_from_slice(&encoded);
        }
        bytes
    }

    /// A payload byte derived from a sequence, so records are distinguishable.
    fn byte_of(seq: u64) -> u8 {
        u8::try_from(seq % 256).expect("in range")
    }

    #[test]
    fn decode_segment_decodes_every_whole_record() {
        let bytes = encode_records(3);
        let records = decode_segment(dummy_path(), &bytes).expect("decodes");

        assert_eq!(records.len(), 3);
        let sequences: Vec<u64> = records.iter().map(|record| record.seq).collect();
        assert_eq!(sequences, [0, 1, 2]);
        assert_eq!(records[2].payload, vec![byte_of(2)]);
        assert_eq!(records[2].timestamp_ns, 2);
    }

    #[test]
    fn decode_segment_stops_at_a_torn_header() {
        let mut bytes = encode_records(1);
        // Append fewer than a header's worth of bytes: a torn trailing write.
        bytes.extend_from_slice(&[0u8; 4]);
        let records = decode_segment(dummy_path(), &bytes).expect("a torn tail is not fatal");
        assert_eq!(records.len(), 1);
    }

    #[test]
    fn decode_segment_stops_at_a_torn_payload() {
        let mut bytes = encode_records(2);
        // Trim the last byte so the final record's trailer is incomplete.
        bytes.pop();
        let records = decode_segment(dummy_path(), &bytes).expect("a torn tail is not fatal");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].seq, 0);
    }

    #[test]
    fn decode_segment_reports_a_corrupt_header() {
        // A full header's worth of zeros has a bad magic, which is corruption
        // rather than a torn write.
        let bytes = [0u8; 28];
        assert!(matches!(
            decode_segment(dummy_path(), &bytes),
            Err(StoreError::Segment { .. })
        ));
    }

    #[test]
    fn decode_segment_reports_a_bad_checksum() {
        let mut bytes = encode_records(1);
        bytes[30] ^= 0xFF; // a payload byte, inside the crc-covered region
        assert!(matches!(
            decode_segment(dummy_path(), &bytes),
            Err(StoreError::Segment { .. })
        ));
    }

    #[test]
    fn decode_segment_of_an_empty_buffer_yields_nothing() {
        assert!(
            decode_segment(dummy_path(), &[])
                .expect("decodes")
                .is_empty()
        );
    }
}
