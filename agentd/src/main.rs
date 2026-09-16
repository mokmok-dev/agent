use agentd::auth::TokenStore;
use agentd::server;
use agentd_events::log::{self, EventLog};
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
        /// The event WebSocket Unix socket. Defaults to `~/.agentd/agentd.sock`.
        #[arg(long)]
        socket: Option<PathBuf>,
        /// The durable JSONL event log. Defaults to `~/.agentd/events.jsonl`.
        #[arg(long)]
        log_path: Option<PathBuf>,
        /// The bearer token file. Defaults to `~/.agentd/tokens.json`.
        #[arg(long)]
        token_file: Option<PathBuf>,
    },
}

#[derive(Debug, Error)]
enum RunError {
    #[error("failed to serve: {0}")]
    Serve(server::ServerError),
    #[error("failed to open the event log: {0}")]
    Log(log::LogError),
    #[error("failed to load the token file: {0}")]
    Auth(agentd::auth::AuthError),
}

/// The directory holding the daemon's socket, log, and token file.
///
/// It is private to the daemon user, so a local user outside the daemon's
/// account cannot reach the socket or read the capability tokens.
fn runtime_dir() -> PathBuf {
    std::env::var_os("HOME").map_or_else(
        || std::env::temp_dir().join("agentd"),
        |home| PathBuf::from(home).join(".agentd"),
    )
}

async fn run() -> Result<(), RunError> {
    let args = Args::parse();

    match args.command {
        Command::Serve {
            socket,
            log_path,
            token_file,
        } => {
            let runtime = runtime_dir();
            let socket = socket.unwrap_or_else(|| runtime.join("agentd.sock"));
            let log_path = log_path.unwrap_or_else(|| runtime.join("events.jsonl"));
            let token_file = token_file.unwrap_or_else(|| runtime.join("tokens.json"));

            let tokens = TokenStore::load(&token_file).map_err(RunError::Auth)?;
            let log = EventLog::open(&log_path).map_err(RunError::Log)?;
            let () = server::run(socket, log, tokens)
                .await
                .map_err(RunError::Serve)?;
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
