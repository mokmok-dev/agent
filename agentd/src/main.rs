use clap::{Parser, Subcommand};
use thiserror::Error;

#[derive(Debug, Parser)]
#[command(
    name = "agentd",
    version = env!("CARGO_PKG_VERSION"),
)]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Echo { message: String },
    Fail,
}

#[derive(Debug, Error)]
enum RunError {
    #[error("always fail")]
    Fail,
}

fn run() -> Result<(), RunError> {
    let args = Args::parse();

    match args.command {
        Command::Echo { message } => {
            println!("{message}");
            Ok(())
        },
        Command::Fail => Err(RunError::Fail),
    }
}

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            std::process::ExitCode::FAILURE
        },
    }
}
