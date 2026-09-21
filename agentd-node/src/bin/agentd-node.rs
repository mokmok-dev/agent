//! A minimal node process for validating the client-side foundation.
//!
//! It connects to the daemon, projects events into a SQLite file, and counts
//! events per `CloudEvents` `type`. A real session would replace the reducer and
//! the filter; the transport, checkpoint, and resume behaviour are the same.

use agentd_events::LogEntry;
use agentd_node::{Node, SqliteError, SqliteProjection, SqliteReducer, TypePrefixes};
use clap::Parser;
use rusqlite::{Connection, Transaction};
use secrecy::zeroize::Zeroizing;
use std::path::{Path, PathBuf};
use thiserror::Error;
use tokio::sync::watch;

#[derive(Debug, Parser)]
#[command(name = "agentd-node", version = env!("CARGO_PKG_VERSION"))]
struct Args {
    /// The daemon's event WebSocket Unix socket.
    #[arg(long, default_value_os_t = agentd_events::paths::default_socket())]
    socket: PathBuf,
    /// The SQLite projection file. Its directory must be writable.
    #[arg(long)]
    db: PathBuf,
    /// A file whose entire contents is the bearer token the daemon expects.
    #[arg(long)]
    token_file: PathBuf,
    /// The node's `CloudEvents` `source` identity.
    #[arg(long, default_value = "urn:mokmokd:node")]
    source: String,
    /// `CloudEvents` `type` prefixes to apply; repeatable. Empty matches all.
    #[arg(long = "type-prefix")]
    type_prefixes: Vec<String>,
    /// Delete the projection before starting, rebuilding it from the log.
    #[arg(long)]
    rebuild: bool,
}

/// Counts events per `type`.
struct EventCounts;
impl SqliteReducer for EventCounts {
    type Error = SqliteError;

    fn migrate(conn: &Connection) -> Result<(), Self::Error> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS event_counts (
                type TEXT PRIMARY KEY,
                count INTEGER NOT NULL
            );",
        )?;
        Ok(())
    }

    fn reduce(
        tx: &Transaction<'_>,
        entry: &LogEntry,
    ) -> Result<(), Self::Error> {
        tx.execute(
            "INSERT INTO event_counts (type, count) VALUES (?1, 1) \
             ON CONFLICT(type) DO UPDATE SET count = count + 1",
            [&entry.event.r#type],
        )?;
        Ok(())
    }
}

/// Errors returned by the binary.
#[derive(Debug, Error)]
enum RunError {
    /// Removing the old projection or reading the token failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// Opening the projection failed.
    #[error("failed to open the projection: {0}")]
    Projection(#[from] SqliteError),
    /// The node run loop failed.
    #[error("the node failed: {0}")]
    Node(#[from] agentd_node::NodeError<SqliteError>),
}

async fn run() -> Result<(), RunError> {
    let args = Args::parse();

    if args.rebuild {
        remove_projection(&args.db)?;
    }

    let projection = SqliteProjection::<EventCounts>::open(&args.db)?;
    let interest = TypePrefixes::new(args.type_prefixes);
    let token = Zeroizing::new(std::fs::read_to_string(&args.token_file)?);
    let mut node = Node::new(args.socket, projection, interest, args.source, token.trim());

    let (sender, shutdown) = watch::channel(false);
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            let _ = sender.send(true);
        }
    });

    node.run(shutdown).await?;
    Ok(())
}

/// Removes the projection file and the sidecars SQLite may leave behind.
fn remove_projection(path: &Path) -> Result<(), std::io::Error> {
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let mut candidate = path.as_os_str().to_os_string();
        candidate.push(suffix);
        match std::fs::remove_file(PathBuf::from(candidate)) {
            Ok(()) => {},
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let telemetry = match agentd_telemetry::init("agentd_node=info") {
        Ok(telemetry) => telemetry,
        Err(error) => {
            eprintln!("{error}");
            return std::process::ExitCode::FAILURE;
        },
    };

    let code = match run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!("{error}");
            std::process::ExitCode::FAILURE
        },
    };
    if let Some(telemetry) = telemetry {
        telemetry.shutdown();
    }
    code
}
