//! `verify <log-file>`: recompute the checksum and hash chain of a WAL file,
//! report the first mismatch, and exit non-zero if the log is not intact.
//!
//! A torn trailing record is reported as truncation to the caller rather than a
//! failure, matching `docs/event-bus/wal.md`'s crash-recovery rule.

use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

use agent::wal::{Error, recover};
use clap::Parser;

/// Verify a WAL file's record framing, `crc32c`, and BLAKE3 hash chain.
#[derive(Debug, Parser)]
#[command(name = "verify", version, about)]
struct Cli {
    /// The log file to verify.
    log: PathBuf,
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    let bytes = match fs::read(&cli.log) {
        Ok(bytes) => bytes,
        Err(error) => {
            eprintln!("verify: cannot read {}: {error}", cli.log.display());
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
