//! `sandbox-supervisor`: run the egress forwarder and the confined command in one
//! network namespace.
//!
//! bubblewrap executes a single process as the namespace's init. For a command
//! that is granted egress, that process is this supervisor: it starts
//! `egress-forward`, waits until it is listening, runs the command, stops the
//! forwarder, and exits with the command's code.
//!
//! Usage:
//! `sandbox-supervisor --forward <path> --socket <path> --port <port> -- <program> [args...]`

use std::process::ExitCode;

use sandbox::supervisor::{SupervisorConfig, run};

fn main() -> ExitCode {
    let config = match SupervisorConfig::parse(std::env::args_os().skip(1)) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("sandbox-supervisor: {error}");
            eprintln!(
                "usage: sandbox-supervisor --forward <path> --socket <path> \
                 --port <port> -- <program> [args...]"
            );
            return ExitCode::FAILURE;
        },
    };

    match run(&config) {
        Ok(outcome) => {
            let code = outcome.exit_code();
            // An exit code is an integer; the process exit code is a `u8`-range
            // value, so a negative or over-large code is clamped.
            ExitCode::from(u8::try_from(code).unwrap_or(1))
        },
        Err(error) => {
            eprintln!("sandbox-supervisor: {error}");
            ExitCode::FAILURE
        },
    }
}
