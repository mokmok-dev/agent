//! The agent node: a sandboxed node that runs the LLM agent loop.
//!
//! It connects to the daemon's event API (`/events`) and inference endpoint
//! (`/inference`) over one Unix domain socket. When an `agent.inbox` event for
//! its conversation arrives, it asks the daemon for a completion, runs the
//! `shell` tool for any tool call, and publishes the finalized messages as
//! `agent.*` events. The whole process is confined by the sandbox the daemon's
//! session manager launched it under, so the commands it spawns inherit the
//! confinement.

use agentd_node::{
    Agent, AgentError, Conversation, ShellLimits, SqliteProjection, conversation_key,
};
use clap::Parser;
use secrecy::zeroize::Zeroizing;
use std::path::{Path, PathBuf};
use std::time::Duration;
use thiserror::Error;
use tokio::sync::watch;
use uuid::Uuid;

#[derive(Debug, Parser)]
#[command(name = "agentd-agent", version = env!("CARGO_PKG_VERSION"))]
struct Args {
    /// The daemon's Unix socket, serving both `/events` and `/inference`.
    #[arg(long, default_value_os_t = agentd_events::paths::default_socket())]
    socket: PathBuf,
    /// The SQLite conversation projection file. Its directory must be writable.
    /// Defaults to `$XDG_DATA_HOME/agentd/agent.db`.
    #[arg(long, default_value_os_t = default_db())]
    db: PathBuf,
    /// A file whose entire contents is the bearer token the daemon expects. The
    /// token needs the read, publish, and infer claims. Defaults to
    /// `$XDG_CONFIG_HOME/agentd/agent.token`.
    #[arg(long, default_value_os_t = default_agent_token())]
    token_file: PathBuf,
    /// The conversation this agent answers. Without it, a new conversation is
    /// created unless `--resume` is given.
    #[arg(long)]
    conversation: Option<String>,
    /// Continue the most recent conversation recorded for `--workdir` instead
    /// of starting a new one. A workdir with no recorded conversation starts a
    /// new one rather than failing, so the session manager may pass this
    /// unconditionally.
    #[arg(long)]
    resume: bool,
    /// The workspace directory the shell tool runs in.
    #[arg(long, default_value_os_t = default_workdir())]
    workdir: PathBuf,
    /// The agent's `CloudEvents` `source` identity.
    #[arg(long, default_value = "urn:mokmokd:agent")]
    source: String,
    /// The model to ask the daemon for (an alias or `provider/model`). Without
    /// it the daemon uses its configured default.
    #[arg(long)]
    model: Option<String>,
    /// Wall-clock seconds a single shell command may run before it is killed.
    #[arg(long, default_value_t = 120)]
    shell_timeout_secs: u64,
    /// Wall-clock seconds a single inference response may take before the turn
    /// is abandoned.
    #[arg(long, default_value_t = 180)]
    inference_timeout_secs: u64,
    /// The largest tool output kept on the result; the rest is truncated.
    #[arg(long, default_value_t = 32 * 1024)]
    max_output_bytes: usize,
    /// Delete the projection before starting, rebuilding it from the log.
    #[arg(long)]
    rebuild: bool,
}

/// The default workspace: the process's current directory.
fn default_workdir() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// The default projection path.
fn default_db() -> PathBuf {
    agentd_events::paths::data_dir().join("agent.db")
}

/// The default agent token file.
fn default_agent_token() -> PathBuf {
    agentd_events::paths::config_dir().join("agent.token")
}

/// Errors returned by the binary.
#[derive(Debug, Error)]
enum RunError {
    /// Removing the old projection or reading the token failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// Opening the projection failed.
    #[error(transparent)]
    Projection(AgentError),
    /// The agent run loop failed.
    #[error(transparent)]
    Agent(AgentError),
}

async fn run() -> Result<(), RunError> {
    let args = Args::parse();

    if args.rebuild {
        remove_projection(&args.db)?;
    }

    let projection =
        SqliteProjection::<Conversation>::open(&args.db).map_err(RunError::Projection)?;
    let conversation = match args.conversation {
        Some(conversation) => conversation,
        // A workdir with no recorded conversation is not an error: the session
        // manager passes `--resume` on every launch, including the first, and
        // failing there would kill a session that has nothing to resume yet.
        None if args.resume => {
            let key = conversation_key(&args.workdir);
            Conversation::latest_conversation(projection.connection(), &key)
                .map_err(RunError::Projection)?
                .unwrap_or_else(|| {
                    tracing::warn!(
                        workdir = %key,
                        "no conversation to resume for this workdir; starting a new one",
                    );
                    Uuid::new_v4().to_string()
                })
        },
        None => Uuid::new_v4().to_string(),
    };
    let token = Zeroizing::new(std::fs::read_to_string(&args.token_file)?);
    let limits = ShellLimits {
        timeout: Duration::from_secs(args.shell_timeout_secs),
        max_output_bytes: args.max_output_bytes,
    };
    let mut agent = Agent::new(
        args.socket,
        projection,
        conversation,
        args.workdir,
        args.source,
        token.trim(),
    )
    .with_limits(limits)
    .with_inference_timeout(Duration::from_secs(args.inference_timeout_secs));
    if let Some(model) = args.model {
        agent = agent.with_model(model);
    }

    let (sender, shutdown) = watch::channel(false);
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            let _ = sender.send(true);
        }
    });

    agent.run(shutdown).await.map_err(RunError::Agent)
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
    // The binary's own diagnostics live under the `agentd_agent` target (the
    // bin name), which `agentd_node=info` alone does not enable; without it a
    // fatal `error!` here is swallowed and the process exits silently.
    let telemetry = match agentd_telemetry::init("agentd_node=info,agentd_agent=info") {
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
