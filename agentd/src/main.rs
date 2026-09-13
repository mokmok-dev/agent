mod eventstore;
mod server;

use agentd_events::EventBus;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use thiserror::Error;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

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
    Serve {
        #[arg(long, default_value = "/tmp/mokmokd.sock")]
        socket: PathBuf,
        #[arg(long, default_value = "/tmp/mokmokd-events.db")]
        db_path: PathBuf,
    },
}

#[derive(Debug, Error)]
enum RunError {
    #[error("failed to serve: {0}")]
    Serve(server::ServerError),
    #[error("failed to open the event store: {0}")]
    Store(eventstore::StoreError),
}

async fn run() -> Result<(), RunError> {
    let args = Args::parse();

    match args.command {
        Command::Serve { socket, db_path } => {
            let bus = EventBus::default();
            eventstore::open(&db_path, &bus).map_err(RunError::Store)?;
            let () = server::run(socket, bus).await.map_err(RunError::Serve)?;
            Ok(())
        },
    }
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("mokmokd=info,tower_http=debug"));
    let json_layer = tracing_subscriber::fmt::layer().json();
    tracing_subscriber::registry()
        .with(env_filter)
        .with(json_layer)
        .init();

    match run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!("{e}");
            std::process::ExitCode::FAILURE
        },
    }
}
