//! A minimal client that publishes one event to the daemon and waits for it to
//! be committed.
//!
//! It exists so the system can be driven from a shell: for example, publish an
//! `agent.inbox` to start an agent turn. It connects to the event API over the
//! Unix domain socket, sends the event, and reports the position the daemon
//! assigned (or the error notice it answered with).

use agentd_events::Event;
use agentd_node::WsClient;
use clap::Parser;
use serde_json::Value;
use std::path::PathBuf;
use std::time::Duration;
use thiserror::Error;

/// How long to wait for the daemon to commit or reject the event.
const TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Parser)]
#[command(name = "agentd-publish", version = env!("CARGO_PKG_VERSION"))]
struct Args {
    /// The daemon's event WebSocket Unix socket.
    #[arg(long, default_value_os_t = agentd_events::paths::default_socket())]
    socket: PathBuf,
    /// A file whose entire contents is the bearer token the daemon expects. The
    /// token needs the publish claim (and read, to observe the commit).
    #[arg(long)]
    token_file: PathBuf,
    /// The `CloudEvents` `type`, for example `agent.inbox`.
    #[arg(long = "type")]
    r#type: String,
    /// The event `data` as JSON.
    #[arg(long)]
    data: String,
    /// The `source` identity to send; the daemon overwrites it with the
    /// authenticated principal's.
    #[arg(long, default_value = "urn:mokmokd:user")]
    source: String,
}

/// Errors returned by the binary.
#[derive(Debug, Error)]
enum RunError {
    /// Reading the token file failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// The `--data` argument was not valid JSON.
    #[error("the --data value is not valid JSON: {0}")]
    Data(#[from] serde_json::Error),
    /// The WebSocket connection failed.
    #[error(transparent)]
    Client(#[from] agentd_node::ClientError),
    /// The daemon did not commit or reject the event in time.
    #[error("timed out waiting for the daemon")]
    Timeout,
    /// The daemon answered with an error instead of committing the event.
    #[error("the daemon rejected the event: {0}")]
    Rejected(String),
}

async fn run() -> Result<(), RunError> {
    let args = Args::parse();
    let data: Value = serde_json::from_str(&args.data)?;
    let token = std::fs::read_to_string(&args.token_file)?;
    let event = Event::new(args.r#type, data);
    let mut client = WsClient::connect(&args.socket, None, token.trim()).await?;
    client.send(&event).await?;

    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(RunError::Timeout);
        }
        let Some(wire) = tokio::time::timeout(remaining, client.next())
            .await
            .map_err(|_| RunError::Timeout)??
        else {
            return Err(RunError::Timeout);
        };
        if wire.event.r#type.starts_with("error.") {
            let detail = wire.event.data["error"]
                .as_str()
                .unwrap_or(&wire.event.r#type)
                .to_string();
            return Err(RunError::Rejected(detail));
        }
        if wire.event.id == event.id {
            let position = wire
                .seq
                .map_or_else(|| String::from("?"), |seq| seq.to_string());
            println!("committed {} at position {position}", wire.event.r#type);
            return Ok(());
        }
    }
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    match run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            std::process::ExitCode::FAILURE
        },
    }
}
