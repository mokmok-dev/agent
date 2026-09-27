//! End-to-end test of the `verify` binary over a real file.
#![expect(
    clippy::unwrap_used,
    reason = "integration test code may panic when a fixture fails"
)]

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use agent::wal::{HASH_LEN, Record, genesis_hash, hash_record};

/// Write a chained log of `count` records to a unique temp path.
fn write_log(
    count: usize,
    tag: &str,
) -> PathBuf {
    let mut log = Vec::new();
    let mut prev = genesis_hash();
    for index in 0..count {
        let seq = u64::try_from(index).unwrap();
        let byte = u8::try_from(index % 256).unwrap();
        let record = Record::new(seq, 0, vec![byte; 8], prev);
        let bytes = record.encode().unwrap();
        prev = hash_record(&bytes);
        log.extend_from_slice(&bytes);
    }

    let path = std::env::temp_dir().join(format!("agent-verify-{tag}-{}.log", std::process::id()));
    fs::write(&path, &log).unwrap();
    path
}

#[test]
fn the_verify_binary_reports_an_intact_chain() {
    let path = write_log(3, "clean");
    let output = Command::new(env!("CARGO_BIN_EXE_verify"))
        .arg(&path)
        .output()
        .unwrap();
    let _ = fs::remove_file(&path);

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("3 records"), "stdout was: {stdout}");
    assert!(stdout.contains("chain intact"), "stdout was: {stdout}");
}

#[test]
fn the_verify_binary_fails_on_a_tampered_record() {
    let path = write_log(3, "tampered");
    let mut bytes = fs::read(&path).unwrap();
    // Flip a payload byte of the first record, inside the crc-covered region.
    bytes[30] ^= 0xFF;
    fs::write(&path, &bytes).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_verify"))
        .arg(&path)
        .output()
        .unwrap();
    let _ = fs::remove_file(&path);

    assert!(!output.status.success());
}

#[test]
fn the_verify_binary_rejects_a_missing_argument() {
    let output = Command::new(env!("CARGO_BIN_EXE_verify")).output().unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("Usage"), "stderr was: {stderr}");
}

#[test]
fn a_chained_record_carries_the_previous_hash() {
    // Guards the chaining contract the CLI relies on: record 2's `prev_hash` is
    // the hash of record 1's bytes.
    let first = Record::new(1, 0, vec![0; 4], genesis_hash())
        .encode()
        .unwrap();
    let first_hash = hash_record(&first);
    let second = Record::new(2, 0, vec![0; 4], first_hash).encode().unwrap();
    let decoded = Record::decode(&second).unwrap();
    assert_eq!(decoded.prev_hash(), &first_hash);
    assert_eq!(HASH_LEN, first_hash.len());
}
