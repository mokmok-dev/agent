//! `verify <log-file>`: recompute the checksum and hash chain of a WAL file,
//! report the first mismatch, and exit non-zero if the log is not intact.
//!
//! A torn trailing record is reported as truncation to the caller rather than a
//! failure, matching `docs/event-bus/wal.md`'s crash-recovery rule.

use std::env;
use std::fs;
use std::process::ExitCode;

use agent::wal::{Error, recover};

fn main() -> ExitCode {
    let mut args = env::args().skip(1);
    let (Some(path), None) = (args.next(), args.next()) else {
        eprintln!("usage: verify <log-file>");
        return ExitCode::FAILURE;
    };

    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) => {
            eprintln!("verify: cannot read {path}: {error}");
            return ExitCode::FAILURE;
        },
    };

    match recover(&bytes) {
        Ok(recovery) => {
            println!(
                "{} records, {} of {} bytes committed",
                recovery.records().len(),
                recovery.committed_len(),
                recovery.total_len(),
            );
            match recovery.head_seq() {
                Some(seq) => println!("head seq: {seq}"),
                None => println!("head seq: (empty)"),
            }
            if recovery.is_clean() {
                println!("chain intact");
            } else {
                println!(
                    "torn trailing record: truncate to {} bytes",
                    recovery.committed_len()
                );
            }
            ExitCode::SUCCESS
        },
        Err(Error::ChainMismatch { seq }) => {
            eprintln!("verify: hash chain is broken at seq {seq}");
            ExitCode::FAILURE
        },
        Err(error) => {
            eprintln!("verify: {error}");
            ExitCode::FAILURE
        },
    }
}
