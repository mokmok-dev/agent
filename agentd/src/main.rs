use agentd::auth::TokenStore;
use agentd::server;
use agentd_events::log::{self, EventLog};
use agentd_inference::{FakeProvider, Provider, ProviderRegistry, ProvidersConfig};
use clap::{Parser, Subcommand};
use secrecy::ExposeSecret as _;
use std::path::PathBuf;
use std::sync::Arc;
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
        /// The protocol bridge for `--session-command`, so a third-party tool
        /// that does not speak `CloudEvents` participates over its own stdio
        /// protocol.
        #[arg(long, value_enum)]
        session_bridge: Option<BridgeKind>,
        /// Restart a crashed session up to this many times.
        #[arg(long, default_value_t = 0)]
        session_max_restarts: u32,
        /// Kill a session after this many seconds.
        #[arg(long)]
        session_lifetime_secs: Option<u64>,
        /// The provider config JSON enabling real models on `/inference`.
        /// Without it the daemon serves a deterministic fake provider.
        #[arg(long)]
        providers_config: Option<PathBuf>,
    },
    /// Verify the event log's hash chain.
    VerifyLog {
        /// The JSONL event log. Defaults to `~/.agentd/events.jsonl`.
        #[arg(long)]
        log_path: Option<PathBuf>,
    },
    /// Create the private runtime directory and capability tokens for a first
    /// run.
    Init {
        /// The config directory holding the token files. Defaults to
        /// `$XDG_CONFIG_HOME/agentd` (or `~/.config/agentd`).
        #[arg(long)]
        config_dir: Option<PathBuf>,
        /// Overwrite an existing token file.
        #[arg(long)]
        force: bool,
    },
}

/// The protocol bridges a supervised session can speak.
#[cfg(feature = "sandbox")]
#[derive(Clone, Copy, Debug, clap::ValueEnum)]
enum BridgeKind {
    /// The Model Context Protocol over stdio (newline-delimited JSON-RPC 2.0).
    Mcp,
    /// The Agent Client Protocol over stdio; the daemon is the client.
    Acp,
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
    #[error("failed to load the provider config: {0}")]
    Providers(#[from] agentd_inference::ConfigError),
    #[error("failed to initialize the runtime directory: {0}")]
    Init(#[from] agentd::init::InitError),
    #[error("the token file {0} does not exist; run `agentd init` to create it")]
    MissingTokens(PathBuf),
}

/// Options for the session manager, parsed from the `serve` arguments.
#[cfg(feature = "sandbox")]
struct SessionOptions {
    command: String,
    policy_path: PathBuf,
    agent_id: String,
    supervision: agentd::session::Supervision,
    bridge: Option<BridgeKind>,
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
    let SessionOptions {
        command,
        agent_id,
        supervision,
        bridge,
        socket,
        ..
    } = options;
    if !policy.network.unix_sockets.contains(&socket) {
        policy.network.unix_sockets.push(socket);
    }
    let manager = agentd::session::SessionManager::new(log.clone(), &policy, command, agent_id)?
        .with_supervision(supervision);
    let manager = match bridge {
        None => manager,
        Some(BridgeKind::Mcp) => {
            manager.with_protocol(Arc::new(agentd::bridge::McpProtocol::default()))
        },
        Some(BridgeKind::Acp) => {
            manager.with_protocol(Arc::new(agentd::bridge::AcpProtocol::default()))
        },
    };
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
            session_bridge,
            providers_config,
        } => {
            let socket = socket.unwrap_or_else(agentd_events::paths::default_socket);
            let log_path = log_path.unwrap_or_else(agentd_events::paths::default_log);
            let token_file = token_file.unwrap_or_else(agentd_events::paths::default_tokens);

            if !token_file.exists() {
                return Err(RunError::MissingTokens(token_file));
            }
            let tokens = TokenStore::load(&token_file).map_err(RunError::Auth)?;
            let log = EventLog::open(&log_path).map_err(RunError::Log)?;

            #[cfg(feature = "sandbox")]
            if let Some(command) = session_command.as_deref() {
                let policy_path = sandbox_policy.ok_or(RunError::MissingSandboxPolicy)?;
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
                        agent_id: session_agent_id,
                        supervision,
                        bridge: session_bridge,
                        socket: socket.clone(),
                    },
                )?;
            }
            #[cfg(feature = "sandbox")]
            if session_command.is_none() && session_bridge.is_some() {
                tracing::warn!("--session-bridge ignored: it requires --session-command");
            }
            #[cfg(not(feature = "sandbox"))]
            {
                if session_command.is_some() {
                    tracing::warn!(
                        "--session-command ignored: this build lacks the `sandbox` feature"
                    );
                }
                let _ = (
                    sandbox_policy,
                    session_agent_id,
                    session_max_restarts,
                    session_lifetime_secs,
                    session_bridge,
                );
            }

            let providers_config = providers_config.or_else(|| {
                let default = agentd_events::paths::default_providers();
                default.exists().then_some(default)
            });
            let provider: Arc<dyn Provider> = match providers_config {
                Some(path) => {
                    let config = ProvidersConfig::load(&path)?;
                    Arc::new(ProviderRegistry::new(config)?)
                },
                None => Arc::new(FakeProvider::default()),
            };
            server::run(socket, log, tokens, provider)
                .await
                .map_err(RunError::Serve)
        },
        Command::VerifyLog { log_path } => {
            let path = log_path.unwrap_or_else(agentd_events::paths::default_log);
            let count = agentd_events::verify_chain(&path).map_err(RunError::Log)?;
            tracing::info!(count, path = %path.display(), "hash chain verified");
            Ok(())
        },
        Command::Init { config_dir, force } => {
            let config_dir = config_dir.unwrap_or_else(agentd_events::paths::config_dir);
            let initialized = agentd::init::init(&config_dir, force).map_err(RunError::Init)?;
            print_initialized(&initialized);
            Ok(())
        },
    }
}

/// Prints the paths that [`agentd::init::init`] created, showing the secrets
/// once so they can be copied into client commands.
fn print_initialized(initialized: &agentd::init::Initialized) {
    println!("initialized {}", initialized.config_dir.display());
    println!("  tokens: {}", initialized.tokens_path.display());
    println!(
        "  providers: {} ({})",
        initialized.providers_path.display(),
        if initialized.providers_created {
            "edit to add your provider and models"
        } else {
            "left as-is"
        }
    );
    println!("  clients (secrets are shown once):");
    for client in &initialized.clients {
        println!(
            "    {}: {}  ({})",
            client.name,
            client.secret.expose_secret(),
            client.path.display()
        );
    }
    println!("start the daemon with:");
    println!(
        "  agentd serve --socket {} --log-path {} --token-file {}",
        agentd_events::paths::default_socket().display(),
        agentd_events::paths::default_log().display(),
        initialized.tokens_path.display(),
    );
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("agentd=info,tower_http=debug"));
    let json_layer = tracing_subscriber::fmt::layer().json();
    tracing_subscriber::registry()
        .with(env_filter)
        .with(json_layer)
        .init();

    match run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            std::process::ExitCode::FAILURE
        },
    }
}
