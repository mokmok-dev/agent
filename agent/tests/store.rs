//! Property tests for the file-backed store. The reference model is the list of
//! payloads written; reopening the directory must reproduce that list in order,
//! whatever segment boundaries rotation chose.

#![expect(
    clippy::unwrap_used,
    reason = "integration test code may panic when the store or a fixture fails"
)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use agent::wal::{Record, Store, genesis_hash, hash_record, recover_from};
use proptest::prelude::*;

/// A unique, empty directory under the system temp dir, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agent-store-prop-{tag}-{}-{unique}",
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

/// Every `segment-*.log` file in `dir`, in sequence order.
fn segment_files(dir: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("log"))
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("segment-"))
        })
        .collect();
    // Segment names are `segment-{first_seq:020}.log`, so a lexicographic sort
    // is a sequence sort.
    paths.sort();
    paths
}

/// Read the payloads back from disk, validating framing and the chain.
fn read_payloads(dir: &Path) -> Vec<Vec<u8>> {
    let mut payloads = Vec::new();
    let mut prev = genesis_hash();
    for path in segment_files(dir) {
        let bytes = fs::read(&path).unwrap();
        let recovery = recover_from(&bytes, prev).unwrap();
        for entry in recovery.records() {
            let start = entry.offset();
            let end = start + entry.byte_len();
            let record = Record::decode(&bytes[start..end]).unwrap();
            payloads.push(record.payload().to_vec());
            prev = hash_record(&bytes[start..end]);
        }
    }
    payloads
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Whatever segment size rotation used, a reopen reproduces the payloads in
    /// order and reports the same head.
    #[test]
    fn a_reopen_reproduces_the_written_payloads(
        payloads in prop::collection::vec(
            prop::collection::vec(any::<u8>(), 0..=64),
            0..48,
        ),
        // A tiny target forces many segment boundaries; a large one keeps a
        // single segment.
        target in 1u64..4096,
    ) {
        let dir = TempDir::new("roundtrip");

        {
            let mut store = Store::with_segment_size(&dir.0, target).unwrap();
            for payload in &payloads {
                store.append(payload).unwrap();
            }
            prop_assert_eq!(store.head_seq(), payloads.len().checked_sub(1).map(|n| n as u64));
        }

        prop_assert_eq!(read_payloads(&dir.0), payloads.as_slice());

        let reopened = Store::open(&dir.0).unwrap();
        prop_assert_eq!(
            reopened.head_seq(),
            payloads.len().checked_sub(1).map(|n| n as u64)
        );
    }

    /// A torn tail never loses an acknowledged record: recovery keeps every
    /// whole record and drops the partial one.
    #[test]
    fn a_torn_tail_keeps_every_acknowledged_record(
        payloads in prop::collection::vec(
            prop::collection::vec(any::<u8>(), 0..=32),
            1..24,
        ),
        cut in 0usize..96,
    ) {
        let dir = TempDir::new("torn");

        {
            let mut store = Store::open(&dir.0).unwrap();
            for payload in &payloads {
                store.append(payload).unwrap();
            }
        }

        // Append a strict prefix of a well-formed next record.
        let segment = segment_files(&dir.0).pop().unwrap();
        let mut bytes = fs::read(&segment).unwrap();
        let committed_len = bytes.len();
        let next = Record::new(999, 0, b"torn".to_vec(), genesis_hash())
            .encode()
            .unwrap();
        bytes.extend_from_slice(&next[..cut.min(next.len() - 1)]);
        fs::write(&segment, &bytes).unwrap();

        let reopened = Store::open(&dir.0).unwrap();
        prop_assert_eq!(
            reopened.head_seq(),
            payloads.len().checked_sub(1).map(|n| n as u64)
        );
        prop_assert_eq!(fs::read(&segment).unwrap().len(), committed_len);
        prop_assert_eq!(read_payloads(&dir.0), payloads);
    }
}
