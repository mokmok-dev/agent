//! The `agentd-client` binary: relay the daemon's event API to a TUI or a
//! browser over a loopback WebSocket.

use agentd_client::{Args, Config, RunError, run};
use clap::Parser as _;

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let telemetry = match agentd_telemetry::init("agentd_client=info") {
        Ok(telemetry) => telemetry,
        Err(error) => {
            eprintln!("{error}");
            return std::process::ExitCode::FAILURE;
        },
    };
    let code = match run_command().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            std::process::ExitCode::FAILURE
        },
    };
    if let Some(telemetry) = telemetry {
        telemetry.shutdown();
    }
    code
}

/// Parses the arguments and serves until a signal.
async fn run_command() -> Result<(), RunError> {
    let args = Args::parse();
    let config = Config::try_from(&args)?;
    run(config).await
}
