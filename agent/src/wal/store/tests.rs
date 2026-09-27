//! Tests for the file-backed store: durability across reopen, torn-tail
//! truncation, segment rotation, and cross-segment chain validation.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use super::*;

/// The `segment-*.log` files in `dir`, sorted by name (that is, by sequence).
fn segment_files(dir: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = fs::read_dir(dir)
        .expect("reads the store directory")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("log"))
        })
        .collect();
    paths.sort();
    paths
}

/// A unique, empty directory under the system temp dir.
fn temp_dir(tag: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir =
        std::env::temp_dir().join(format!("agent-store-{tag}-{}-{unique}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    dir
}

/// A cleanup guard so a failing assertion does not leak temp directories.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        Self(temp_dir(tag))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn an_empty_store_has_no_head() {
    let dir = TempDir::new("empty");
    let store = Store::open(dir.path()).expect("an empty directory opens");
    assert_eq!(store.head_seq(), None);
    assert_eq!(store.head_hash(), genesis_hash());
}

#[test]
fn appended_records_survive_a_reopen() {
    let dir = TempDir::new("reopen");
    let mut store = Store::open(dir.path()).expect("opens");
    let first = store.append(b"hello").expect("appends");
    let second = store.append(b"world").expect("appends");
    assert_eq!(first.seq, 0);
    assert_eq!(second.seq, 1);
    let head_hash = store.head_hash();
    drop(store);

    let reopened = Store::open(dir.path()).expect("reopens");
    assert_eq!(reopened.head_seq(), Some(1));
    assert_eq!(reopened.head_hash(), head_hash);
}

#[test]
fn a_batch_gets_contiguous_sequence_numbers() {
    let dir = TempDir::new("batch");
    let mut store = Store::open(dir.path()).expect("opens");
    store.append(b"first").expect("appends");

    let batch = store
        .append_batch(&[b"a".as_slice(), b"b", b"c"])
        .expect("appends the batch");

    let sequences: Vec<u64> = batch.iter().map(|committed| committed.seq).collect();
    assert_eq!(sequences, [1, 2, 3]);
    assert_eq!(store.head_seq(), Some(3));

    // The whole batch is durable after the single commit.
    let reopened = Store::open(dir.path()).expect("reopens");
    assert_eq!(reopened.head_seq(), Some(3));
}

#[test]
fn an_empty_batch_commits_nothing() {
    let dir = TempDir::new("empty-batch");
    let mut store = Store::open(dir.path()).expect("opens");
    let committed = store.append_batch(&[]).expect("an empty batch is a no-op");
    assert!(committed.is_empty());
    assert_eq!(store.head_seq(), None);
}

#[test]
fn a_torn_trailing_record_is_truncated_on_open() {
    let dir = TempDir::new("torn");
    let mut store = Store::open(dir.path()).expect("opens");
    store.append(b"one").expect("appends");
    store.append(b"two").expect("appends");
    drop(store);

    // Append a prefix of a well-formed next record, as a crash mid-write leaves.
    let segment = dir.path().join(segment_name(0));
    let mut bytes = fs::read(&segment).expect("reads the segment");
    let committed_len = bytes.len();
    let next = Record::new(2, 0, b"three".to_vec(), genesis_hash())
        .encode()
        .expect("encodes");
    bytes.extend_from_slice(&next[..next.len() / 2]);
    fs::write(&segment, &bytes).expect("writes the torn tail");

    let store = Store::open(dir.path()).expect("a torn tail is not fatal");
    assert_eq!(store.head_seq(), Some(1));

    // The truncation is durable: the file is back to its committed length.
    assert_eq!(fs::read(&segment).expect("reads").len(), committed_len);
}

#[test]
fn rotation_creates_a_new_segment_and_preserves_the_chain() {
    let dir = TempDir::new("rotation");
    // A target between one and two records forces a rotation on the second.
    let one_record = 6 + crate::wal::RECORD_OVERHEAD;
    let mut store = Store::with_segment_size(dir.path(), one_record as u64 + 1).expect("opens");

    // Eight records over a tiny target produces several segments.
    for _ in 0..8 {
        store.append(b"payload").expect("appends");
    }
    assert_eq!(store.head_seq(), Some(7));
    drop(store);

    let segments: Vec<_> = fs::read_dir(dir.path())
        .expect("reads the dir")
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("segment-"))
        .collect();
    assert!(
        segments.len() > 1,
        "a small target must rotate, found {} segment(s)",
        segments.len()
    );

    // Recovery validates the chain across every segment boundary.
    let report = verify_dir(dir.path()).expect("the rotated log validates");
    assert_eq!(report.records, 8);
    assert_eq!(report.head_seq, Some(7));
    assert!(report.is_clean);
}

#[test]
fn verify_reports_a_clean_log() {
    let dir = TempDir::new("verify-clean");
    let mut store = Store::open(dir.path()).expect("opens");
    store.append(b"one").expect("appends");
    store.append(b"two").expect("appends");
    drop(store);

    let report = verify_dir(dir.path()).expect("validates");
    assert_eq!(report.segments, 1);
    assert_eq!(report.records, 2);
    assert_eq!(report.head_seq, Some(1));
    assert!(report.is_clean);
    assert_eq!(report.committed_len, report.total_len);
}

#[test]
fn verify_reports_a_torn_tail_without_failing() {
    let dir = TempDir::new("verify-torn");
    let mut store = Store::open(dir.path()).expect("opens");
    store.append(b"one").expect("appends");
    store.append(b"two").expect("appends");
    drop(store);

    // Append a prefix of a well-formed next record, then verify without
    // reopening so the torn tail is still on disk.
    let segment = dir.path().join(segment_name(0));
    let mut bytes = fs::read(&segment).expect("reads");
    let next = Record::new(2, 0, b"three".to_vec(), genesis_hash())
        .encode()
        .expect("encodes");
    bytes.extend_from_slice(&next[..next.len() / 2]);
    fs::write(&segment, &bytes).expect("writes");

    let report = verify_dir(dir.path()).expect("a torn tail is not fatal");
    assert!(!report.is_clean);
    assert_eq!(report.head_seq, Some(1));
    assert_eq!(report.records, 2);
}

#[test]
fn verify_rejects_a_tampered_record() {
    let dir = TempDir::new("verify-tampered");
    let mut store = Store::open(dir.path()).expect("opens");
    store.append(b"one").expect("appends");
    store.append(b"two").expect("appends");
    drop(store);

    let segment = dir.path().join(segment_name(0));
    let mut bytes = fs::read(&segment).expect("reads");
    bytes[30] ^= 0xFF; // a payload byte, inside the crc-covered region
    fs::write(&segment, &bytes).expect("writes");

    assert!(matches!(
        verify_dir(dir.path()),
        Err(StoreError::Segment { .. })
    ));
}

#[test]
fn a_torn_non_final_segment_is_fatal() {
    let dir = TempDir::new("torn-nonfinal");
    let one_record = 6 + crate::wal::RECORD_OVERHEAD;
    let mut store = Store::with_segment_size(dir.path(), one_record as u64 + 1).expect("opens");
    for _ in 0..4 {
        store.append(b"payload").expect("appends");
    }
    drop(store);

    // Corrupt the first (non-final) segment's tail so it ends in a torn record.
    let mut names: Vec<PathBuf> = fs::read_dir(dir.path())
        .expect("reads")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "log"))
        .collect();
    names.sort();
    let first = &names[0];
    let mut bytes = fs::read(first).expect("reads");
    bytes.truncate(bytes.len() - 1);
    fs::write(first, &bytes).expect("writes");

    assert!(matches!(
        Store::open(dir.path()),
        Err(StoreError::TornNonFinalSegment { .. })
    ));
}

#[test]
fn segment_names_round_trip_and_reject_lookalikes() {
    assert_eq!(segment_name(0), "segment-00000000000000000000.log");
    assert_eq!(segment_name(42), "segment-00000000000000000042.log");
    assert_eq!(parse_segment_name(&segment_name(1234)), Some(1234));
    assert_eq!(parse_segment_name("segment-42.log"), None);
    assert_eq!(parse_segment_name("segment-0000000000000000000x.log"), None);
    assert_eq!(parse_segment_name("notes.txt"), None);
}

#[test]
fn a_non_segment_file_is_ignored() {
    let dir = TempDir::new("ignored");
    let mut store = Store::open(dir.path()).expect("opens");
    store.append(b"one").expect("appends");
    drop(store);

    fs::write(dir.path().join("README"), b"not a segment").expect("writes");

    let report = verify_dir(dir.path()).expect("ignores non-segment files");
    assert_eq!(report.records, 1);
    assert_eq!(report.segments, 1);
}

#[test]
fn the_segment_size_accessor_reports_the_configured_target() {
    let dir = TempDir::new("segment-size");
    let store = Store::with_segment_size(dir.path(), 4096).expect("opens");
    assert_eq!(store.segment_size(), 4096);
    assert_eq!(store.dir(), dir.path());
}

#[test]
fn the_default_segment_size_is_64_mib() {
    assert_eq!(DEFAULT_SEGMENT_SIZE, 67_108_864);
}

#[test]
fn a_record_that_exactly_fills_the_target_does_not_rotate() {
    let dir = TempDir::new("exact-fit");
    let record_len = 7 + crate::wal::RECORD_OVERHEAD;
    // Two records exactly fill the target, so the second must stay in the first
    // segment and only the third rotates.
    let mut store = Store::with_segment_size(dir.path(), record_len as u64 * 2).expect("opens");

    store.append(b"payload").expect("appends"); // 1 record: len = record_len
    store.append(b"payload").expect("appends"); // 2 records: len = target exactly
    assert_eq!(
        segment_files(dir.path()).len(),
        1,
        "two records fit exactly"
    );

    store.append(b"payload").expect("appends"); // 3 records: must rotate
    assert_eq!(segment_files(dir.path()).len(), 2, "the third rotates");
    assert!(segment_files(dir.path())[0].ends_with(segment_name(0)));
    assert!(segment_files(dir.path())[1].ends_with(segment_name(2)));
}

#[test]
fn the_first_record_of_an_empty_segment_stays_in_that_segment() {
    let dir = TempDir::new("first-record");
    // The target is far larger than one record, so the first record must not
    // trigger a rotation into a differently named segment.
    let mut store = Store::with_segment_size(dir.path(), 4096).expect("opens");
    store.append(b"payload").expect("appends");

    let files = segment_files(dir.path());
    assert_eq!(files.len(), 1);
    assert!(files[0].ends_with(segment_name(0)));
}

#[test]
fn an_oversized_first_record_does_not_create_an_empty_segment() {
    let dir = TempDir::new("oversized-first");
    // The target is smaller than a single record. The first record must be
    // written into the segment that was just opened, in a segment named for it.
    let mut store = Store::with_segment_size(dir.path(), 1).expect("opens");
    store
        .append(b"payload larger than the target")
        .expect("appends");

    let files = segment_files(dir.path());
    assert_eq!(files.len(), 1, "no empty segment is created");
    assert!(files[0].ends_with(segment_name(0)));
    assert!(fs::metadata(&files[0]).expect("metadata").len() > 0);
}

#[test]
fn a_write_failure_poisons_the_store_until_reopen() {
    let dir = TempDir::new("poison");
    let mut store = Store::open(dir.path()).expect("opens");
    store.append(b"ok").expect("appends");
    drop(store);

    // Reopen, arm the failure, and confirm the append fails and poisons.
    let mut store = Store::open(dir.path()).expect("reopens");
    store.fail_next_write = true;
    assert!(matches!(store.append(b"fails"), Err(StoreError::Io(_))));
    assert!(matches!(store.append(b"again"), Err(StoreError::Poisoned)));
    // The acknowledged head is unchanged and still readable.
    assert_eq!(store.head_seq(), Some(0));
    drop(store);

    // Reopening truncates the partial bytes and clears the poison.
    let mut store = Store::open(dir.path()).expect("reopens after a torn write");
    assert_eq!(store.head_seq(), Some(0));
    store.append(b"resumed").expect("appends after reopen");
    assert_eq!(store.head_seq(), Some(1));
}
