//! The Open Knowledge Format check that CI runs over `docs/spec`.
//!
//! The bundle is policy, so the check enforces exactly what the format's
//! conformance section says and reports everything softer as a warning. The
//! parser work lives in `spec`, which is pure, so its behaviour is tested
//! against strings rather than against the repository's files.

mod spec;

use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let Some(command) = args.next() else {
        eprintln!("usage: cargo xtask spec-check [path]");
        return ExitCode::from(2);
    };

    if command != "spec-check" {
        eprintln!("unknown command {command}. usage: cargo xtask spec-check [path]");
        return ExitCode::from(2);
    }

    let root = args
        .next()
        .map_or_else(|| PathBuf::from("docs/spec"), PathBuf::from);
    match spec::check_bundle(&root) {
        Ok(report) => report.print(&root),
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        },
    }
}
