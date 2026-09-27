//! `verify <path>`: recompute the checksum and hash chain of a WAL.
//!
//! `path` may be a single log file or a store directory. A directory is
//! verified segment by segment, across segment boundaries. The command reports
//! the first mismatch and exits non-zero if the log is not intact.
//!
//! A torn trailing record is reported as truncation to the caller rather than a
//! failure, matching `docs/event-bus/wal.md`'s crash-recovery rule.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use agent::wal::{Error, recover, verify_dir};
use clap::Parser;

/// Verify a WAL file's, or store directory's, framing, `crc32c`, and BLAKE3
/// hash chain.
#[derive(Debug, Parser)]
#[command(name = "verify", version, about)]
struct Cli {
    /// The log file or store directory to verify.
    path: PathBuf,
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    match fs::metadata(&cli.path) {
        Ok(metadata) if metadata.is_dir() => verify_store(&cli.path),
        Ok(_) => verify_file(&cli.path),
        Err(error) => {
            eprintln!("verify: cannot stat {}: {error}", cli.path.display());
            ExitCode::FAILURE
        },
    }
}

/// Verify a single log file.
fn verify_file(path: &Path) -> ExitCode {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) => {
            eprintln!("verify: cannot read {}: {error}", path.display());
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

/// Verify a store directory segment by segment.
fn verify_store(dir: &Path) -> ExitCode {
    match verify_dir(dir) {
        Ok(report) => {
            println!(
                "{} segments, {} records, {} of {} bytes committed",
                report.segments, report.records, report.committed_len, report.total_len,
            );
            match report.head_seq {
                Some(seq) => println!("head seq: {seq}"),
                None => println!("head seq: (empty)"),
            }
            if report.is_clean {
                println!("chain intact");
            } else {
                println!(
                    "torn trailing record: truncate to {} bytes",
                    report.committed_len
                );
            }
            ExitCode::SUCCESS
        },
        Err(error) => {
            eprintln!("verify: {error}");
            ExitCode::FAILURE
        },
    }
}
