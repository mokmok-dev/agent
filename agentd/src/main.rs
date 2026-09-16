use agentd::auth::TokenStore;
use agentd::server;
use agentd_events::log::{self, EventLog};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
#[cfg(feature = "sandbox")]
use std::time::Duration;
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
        /// Run this configured command in a sandbox when a `session.requested`
        /// event arrives. Requires `--sandbox-policy` and a build with the
        /// `sandbox` feature.
        #[arg(long)]
        session_command: Option<String>,
        /// The sandbox policy JSON used for `--session-command`.
        #[arg(long)]
        sandbox_policy: Option<PathBuf>,
        /// The agent id recorded on session events.
        #[arg(long, default_value = "urn:mokmokd:session")]
        session_agent_id: String,
        /// Restart a crashed session up to this many times.
        #[arg(long, default_value_t = 0)]
        session_max_restarts: u32,
        /// Kill a session after this many seconds.
        #[arg(long)]
        session_lifetime_secs: Option<u64>,
    },
    /// Verify the event log's hash chain.
    VerifyLog {
        /// The JSONL event log. Defaults to `~/.agentd/events.jsonl`.
        #[arg(long)]
        log_path: Option<PathBuf>,
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
    #[cfg(feature = "sandbox")]
    #[error("--session-command requires --sandbox-policy")]
    MissingSandboxPolicy,
    #[cfg(feature = "sandbox")]
    #[error("failed to read the sandbox policy: {0}")]
    Io(#[from] std::io::Error),
    #[cfg(feature = "sandbox")]
    #[error("failed to parse the sandbox policy: {0}")]
    Policy(#[from] serde_json::Error),
    #[cfg(feature = "sandbox")]
    #[error("failed to build the session manager: {0}")]
    Session(#[from] agentd::session::SessionError),
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

/// Options for the session manager, parsed from the `serve` arguments.
#[cfg(feature = "sandbox")]
struct SessionOptions {
    command: String,
    policy_path: PathBuf,
    agent_id: String,
    supervision: agentd::session::Supervision,
    /// The daemon's own event socket, granted to the session's policy so the
    /// launched node can reach the daemon it is supervised by.
    socket: PathBuf,
}

/// Starts the session manager and a shutdown watcher for it.
#[cfg(feature = "sandbox")]
fn start_session_manager(
    log: &EventLog,
    options: SessionOptions,
) -> Result<(), RunError> {
    let mut policy: agentd_sandbox::Policy =
        serde_json::from_str(&std::fs::read_to_string(&options.policy_path)?)?;
    if !policy.network.unix_sockets.contains(&options.socket) {
        policy.network.unix_sockets.push(options.socket.clone());
    }
    let manager = agentd::session::SessionManager::new(
        log.clone(),
        &policy,
        options.command,
        options.agent_id,
    )?
    .with_supervision(options.supervision);
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        if let Err(error) = manager.run(shutdown_rx).await {
            tracing::error!(%error, "the session manager stopped");
        }
    });
    tokio::spawn(async move {
        let Ok(mut interrupt) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
        else {
            return;
        };
        let Ok(mut terminate) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        else {
            return;
        };
        tokio::select! {
            _ = interrupt.recv() => {},
            _ = terminate.recv() => {},
        }
        let _ = shutdown_tx.send(true);
    });
    Ok(())
}

async fn run() -> Result<(), RunError> {
    let args = Args::parse();

    match args.command {
        Command::Serve {
            socket,
            log_path,
            token_file,
            session_command,
            sandbox_policy,
            session_agent_id,
            session_max_restarts,
            session_lifetime_secs,
        } => {
            let runtime = runtime_dir();
            let socket = socket.unwrap_or_else(|| runtime.join("agentd.sock"));
            let log_path = log_path.unwrap_or_else(|| runtime.join("events.jsonl"));
            let token_file = token_file.unwrap_or_else(|| runtime.join("tokens.json"));

            let tokens = TokenStore::load(&token_file).map_err(RunError::Auth)?;
            let log = EventLog::open(&log_path).map_err(RunError::Log)?;

            #[cfg(feature = "sandbox")]
            if let Some(command) = session_command.as_deref() {
                let policy_path = sandbox_policy
                    .clone()
                    .ok_or(RunError::MissingSandboxPolicy)?;
                let supervision = agentd::session::Supervision {
                    max_restarts: session_max_restarts,
                    restart_backoff: Duration::from_secs(1),
                    lifetime: session_lifetime_secs.map(Duration::from_secs),
                };
                start_session_manager(
                    &log,
                    SessionOptions {
                        command: command.to_string(),
                        policy_path,
                        agent_id: session_agent_id.clone(),
                        supervision,
                        socket: socket.clone(),
                    },
                )?;
            }
            #[cfg(not(feature = "sandbox"))]
            if session_command.is_some() {
                tracing::warn!("--session-command ignored: this build lacks the `sandbox` feature");
            }
            let _ = (
                session_command,
                sandbox_policy,
                session_agent_id,
                session_max_restarts,
                session_lifetime_secs,
            );

            let () = server::run(socket, log, tokens)
                .await
                .map_err(RunError::Serve)?;
            Ok(())
        },
        Command::VerifyLog { log_path } => {
            let path = log_path.unwrap_or_else(|| runtime_dir().join("events.jsonl"));
            match agentd_events::verify_chain(&path) {
                Ok(count) => {
                    tracing::info!(count, path = %path.display(), "hash chain verified");
                    Ok(())
                },
                Err(error) => Err(RunError::Log(error)),
            }
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
