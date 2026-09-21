use agentd::auth::TokenStore;
use agentd::server;
#[cfg(feature = "sandbox")]
use agentd_events::Event;
use agentd_events::log::{self, EventLog};
use agentd_inference::{FakeProvider, Provider, ProviderRegistry, ProvidersConfig};
use clap::{Parser, Subcommand};
use secrecy::ExposeSecret as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(feature = "sandbox")]
use std::time::Duration;
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
    Serve(Box<ServeArgs>),
    /// Bring the daemon and a sandboxed agent up in one shot.
    #[cfg(feature = "sandbox")]
    Up(Box<agentd::up::UpArgs>),
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
///
/// Defined without the `sandbox` feature too, so the CLI parses identically on
/// a build that cannot act on it; the value is ignored when the feature is off.
#[derive(Clone, Copy, Debug, clap::ValueEnum)]
enum BridgeKind {
    /// The Model Context Protocol over stdio (newline-delimited JSON-RPC 2.0).
    Mcp,
    /// The Agent Client Protocol over stdio; the daemon is the client.
    Acp,
}

/// The `serve` arguments, boxed to keep the `Command` enum small.
#[derive(Debug, clap::Args)]
#[command(name = "serve")]
struct ServeArgs {
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
    /// A destination the session's CONNECT proxy may tunnel to, `host:port`.
    /// Repeatable; empty leaves egress denied. The proxy is started and the
    /// session policy is pointed at it automatically; it can be combined with
    /// `--session-loopback` (an agent that also binds its own server).
    #[arg(long = "session-egress", value_name = "HOST:PORT")]
    session_egress: Vec<String>,
    /// Grant the session free loopback (its own server and client), for an
    /// agent that binds an ephemeral port and talks to it. Needs bubblewrap on
    /// Linux.
    #[arg(long = "session-loopback")]
    session_loopback: bool,
    /// Ask an approver (an authority client) before tunnelling to a `host:port`
    /// that is not on a static `--session-egress` rule. The request is a
    /// `session.egress.requested` event, granted or denied by a
    /// `session.egress.granted`/`denied` under the same id; no decision within
    /// this many seconds denies. Without this flag unknown destinations are
    /// denied outright.
    #[arg(long = "session-egress-approval-secs", value_name = "SECS")]
    session_egress_approval_secs: Option<u64>,
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
    #[cfg(feature = "sandbox")]
    #[error("--session-egress must be host:port: {0}")]
    Egress(String),
    #[cfg(feature = "sandbox")]
    #[error("failed to start the egress proxy: {0}")]
    Proxy(std::io::Error),
    #[error("failed to load the provider config: {0}")]
    Providers(#[from] agentd_inference::ConfigError),
    /// The model an agent will ask for does not resolve, raised at startup by
    /// `up` so a bad config fails before anything runs.
    #[cfg(feature = "sandbox")]
    #[error(
        "no usable model at startup: {reason} \
         (checked {path}); add the model under \"models\", fix \"default_model\", or pass \
         --model naming a configured alias or a `provider/model` pair"
    )]
    Model {
        /// Why resolution failed.
        reason: String,
        /// The provider config that was checked.
        path: PathBuf,
    },
    #[error("failed to initialize the runtime directory: {0}")]
    Init(#[from] agentd::init::InitError),
    #[error("the token file {0} does not exist; run `agentd init` to create it")]
    MissingTokens(PathBuf),
    #[cfg(feature = "sandbox")]
    #[error("failed to derive the one-shot launch: {0}")]
    Up(#[from] agentd::up::UpError),
    #[cfg(feature = "sandbox")]
    #[error("a background task failed: {0}")]
    Task(String),
}

/// Parses `host:port` egress destinations from the CLI, honoring a bracketed
/// IPv6 literal (`[::1]:443`).
#[cfg(feature = "sandbox")]
fn parse_egress(raw: &[String]) -> Result<Vec<agentd_sandbox::HostPort>, RunError> {
    raw.iter()
        .map(|entry| {
            let authority = entry.as_str();
            let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
                let (host, port) = rest
                    .split_once(']')
                    .and_then(|(host, port)| Some((host, port.strip_prefix(':')?)))
                    .ok_or_else(|| RunError::Egress(entry.clone()))?;
                (host, port)
            } else {
                authority
                    .rsplit_once(':')
                    .ok_or_else(|| RunError::Egress(entry.clone()))?
            };
            let port = port.parse().map_err(|_| RunError::Egress(entry.clone()))?;
            if host.is_empty() || port == 0 {
                return Err(RunError::Egress(entry.clone()));
            }
            Ok(agentd_sandbox::HostPort {
                host: host.to_string(),
                port,
            })
        })
        .collect()
}

#[cfg(all(test, feature = "sandbox"))]
mod egress_tests {
    use super::parse_egress;

    #[test]
    fn parses_hostnames_and_ipv6_literals() {
        let parsed = parse_egress(&[
            String::from("api.example.com:443"),
            String::from("[::1]:9000"),
        ])
        .expect("valid destinations");

        assert_eq!(parsed[0].host, "api.example.com");
        assert_eq!(parsed[0].port, 443);
        assert_eq!(parsed[1].host, "::1");
        assert_eq!(parsed[1].port, 9000);
    }

    #[test]
    fn rejects_a_destination_without_a_port() {
        assert!(parse_egress(&[String::from("api.example.com")]).is_err());
        assert!(parse_egress(&[String::from("[::1]")]).is_err());
        assert!(parse_egress(&[String::from("host:notaport")]).is_err());
    }
}

/// Options for the session manager, parsed from the `serve` arguments.
#[cfg(feature = "sandbox")]
struct SessionOptions {
    command: String,
    policy_path: PathBuf,
    agent_id: String,
    supervision: agentd::session::Supervision,
    bridge: Option<BridgeKind>,
    /// The `host:port` destinations the session's proxy may tunnel to. Empty
    /// leaves egress denied (unless approval is enabled).
    egress: Vec<agentd_sandbox::HostPort>,
    /// How long to wait for an approver before denying a destination that is not
    /// on the static allowlist. `None` denies unknown destinations outright.
    egress_approval: Option<Duration>,
    /// Whether the session may use loopback freely (an ACP agent's internal
    /// HTTP server).
    loopback: bool,
    /// The daemon's own event socket, granted to the session's policy so the
    /// launched node can reach the daemon it is supervised by.
    socket: PathBuf,
}

/// Starts the session manager and a shutdown watcher for it.
#[cfg(feature = "sandbox")]
async fn start_session_manager(
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
        egress,
        egress_approval,
        loopback,
        socket,
        ..
    } = options;
    if !policy.network.unix_sockets.contains(&socket) {
        policy.network.unix_sockets.push(socket);
    }
    policy.network.loopback |= loopback;
    // With egress, start the daemon's CONNECT proxy and let it pick the
    // transport the host's capabilities allow: a Unix socket inside a private
    // network namespace, or loopback TCP where there is no namespace (macOS).
    // Linux without bubblewrap fails closed rather than downgrading. It also
    // injects the proxy env and the `NO_PROXY` that keeps the agent's own
    // loopback off the tunnel (see `docs/egress.md`).
    //
    // A proxy exists when there is a static allowlist or an approver to consult;
    // with neither, egress is denied outright.
    let proxy = if egress.is_empty() && egress_approval.is_none() {
        None
    } else {
        let mut egress_config = agentd::proxy::Egress::new(egress);
        if let Some(timeout) = egress_approval {
            egress_config = egress_config.with_approver(log.clone(), timeout);
        }
        Some(
            agentd::proxy::Proxy::start_for_policy(&mut policy, egress_config)
                .await
                .map_err(RunError::Proxy)?,
        )
    };
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
        if let Some(proxy) = proxy {
            proxy.stop();
        }
        let _ = shutdown_tx.send(true);
    });
    Ok(())
}

/// Runs the `serve` subcommand.
async fn serve_command(args: ServeArgs) -> Result<(), RunError> {
    let ServeArgs {
        socket,
        log_path,
        token_file,
        session_command,
        sandbox_policy,
        session_agent_id,
        session_egress,
        session_loopback,
        session_egress_approval_secs,
        session_max_restarts,
        session_lifetime_secs,
        session_bridge,
        providers_config,
    } = args;
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
        let egress = parse_egress(&session_egress)?;
        start_session_manager(
            &log,
            SessionOptions {
                command: command.to_string(),
                policy_path,
                agent_id: session_agent_id,
                supervision,
                bridge: session_bridge,
                egress,
                egress_approval: session_egress_approval_secs.map(Duration::from_secs),
                loopback: session_loopback,
                socket: socket.clone(),
            },
        )
        .await?;
    }
    #[cfg(feature = "sandbox")]
    if session_command.is_none()
        && (session_bridge.is_some() || !session_egress.is_empty() || session_loopback)
    {
        tracing::warn!(
            "--session-bridge/--session-egress/--session-loopback ignored: they require \
             --session-command"
        );
    }
    #[cfg(not(feature = "sandbox"))]
    {
        if session_command.is_some() {
            tracing::warn!("--session-command ignored: this build lacks the `sandbox` feature");
        }
        let _ = (
            sandbox_policy,
            session_agent_id,
            session_max_restarts,
            session_lifetime_secs,
            session_bridge,
            session_egress,
            session_loopback,
            session_egress_approval_secs,
        );
    }

    let provider = load_provider(providers_config.as_deref())?;
    server::run(socket, log, tokens, provider)
        .await
        .map_err(RunError::Serve)
}

/// The provider registry `providers_config` names, or `None` when neither an
/// explicit path nor the default one exists.
///
/// The path is returned with the registry so a caller that rejects the config
/// can name the file.
fn provider_registry(
    providers_config: Option<&Path>
) -> Result<Option<(PathBuf, ProviderRegistry)>, RunError> {
    let Some(path) = providers_config.map_or_else(
        || {
            let default = agentd_events::paths::default_providers();
            default.exists().then_some(default)
        },
        |path| Some(path.to_path_buf()),
    ) else {
        tracing::warn!(
            "no provider config: serving the deterministic fake provider; \
             `agentd init` writes a template to edit"
        );
        return Ok(None);
    };
    let config = ProvidersConfig::load(&path)?;
    Ok(Some((path, ProviderRegistry::new(config)?)))
}

/// Loads the provider for `serve`, which resolves the model per request.
fn load_provider(providers_config: Option<&Path>) -> Result<Arc<dyn Provider>, RunError> {
    Ok(match provider_registry(providers_config)? {
        Some((_path, registry)) => Arc::new(registry),
        None => Arc::new(FakeProvider::default()),
    })
}

/// Loads the provider for `up`, proving `model` resolves against it first.
///
/// `serve` resolves the model per request, so it must serve a config it cannot
/// resolve a model for; `up` launches an agent that asks for one specific model,
/// and a configuration that cannot answer that request should stop the launch
/// with the config file named, not surface after a prompt as an
/// `agent.turn.failed` event.
#[cfg(feature = "sandbox")]
fn load_provider_for(
    providers_config: Option<&Path>,
    model: Option<&str>,
) -> Result<Arc<dyn Provider>, RunError> {
    let Some((path, registry)) = provider_registry(providers_config)? else {
        return Ok(Arc::new(FakeProvider::default()));
    };
    registry.resolve(model).map_err(|error| RunError::Model {
        reason: error.to_string(),
        path,
    })?;
    Ok(Arc::new(registry))
}

async fn run() -> Result<(), RunError> {
    let args = Args::parse();

    match args.command {
        Command::Serve(serve) => serve_command(*serve).await,
        #[cfg(feature = "sandbox")]
        Command::Up(up) => up_command(*up).await,
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

/// Runs the `up` subcommand: serve the daemon and supervise a sandboxed agent
/// derived from a single `--workdir`.
///
/// Everything else the launch needs — the sandbox policy and the agent's
/// command line — is derived from the workspace and the daemon's own paths (see
/// [`agentd::up`]); the operator's only real choice is the workspace.
#[cfg(feature = "sandbox")]
async fn up_command(args: agentd::up::UpArgs) -> Result<(), RunError> {
    let layout = agentd::up::UpPaths::resolve(&args)?;
    let policy = layout.policy()?;
    let command = layout.agent_command(args.model.as_deref(), args.resume);

    let socket = layout.socket.clone();
    let log_path = args
        .log_path
        .clone()
        .unwrap_or_else(agentd_events::paths::default_log);
    // The token file the daemon loads is the one the policy denies to the agent
    // (`UpPaths::daemon_token`), resolved once so the two cannot disagree.
    let token_file = layout.daemon_token.clone();

    if !token_file.exists() {
        return Err(RunError::MissingTokens(token_file));
    }
    let tokens = TokenStore::load(&token_file).map_err(RunError::Auth)?;
    let log = EventLog::open(&log_path).map_err(RunError::Log)?;
    // Loaded before any task is spawned so a bad provider config fails with
    // nothing running.
    let provider = load_provider_for(args.providers_config.as_deref(), args.model.as_deref())?;

    // Claim the single-instance boundary before anything else touches shared
    // state. The manager's startup reconciliation *fails* every session the log
    // shows as still active, on the assumption that this daemon is the one
    // taking over from a crashed predecessor. A second `agentd up` that reached
    // reconciliation while another instance was serving would therefore fail the
    // live instance's session; binding first makes "already running" a failure
    // that changes nothing.
    let listener = server::bind(socket).await.map_err(RunError::Serve)?;

    tracing::info!(
        workdir = %layout.workdir.display(),
        session_db = %layout.session_db.display(),
        agent = %layout.agent_binary.display(),
        "starting a supervised agent session",
    );

    // Start the manager before the server so the session is launched against a
    // subscribed, reconciled manager; `run_ready` signals once the startup
    // snapshot is taken, so the kickoff below is a live event rather than one
    // reconciliation would fail as a leftover.
    let manager =
        agentd::session::SessionManager::new(log.clone(), &policy, command, SESSION_AGENT_ID)?
            .with_workdir(layout.workdir.clone())
            .with_supervision(agentd::session::Supervision {
                max_restarts: args.session_max_restarts,
                restart_backoff: Duration::from_secs(1),
                lifetime: args.session_lifetime_secs.map(Duration::from_secs),
            });
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let manager_task = tokio::spawn(async move {
        if let Err(error) = manager.run_ready(shutdown_rx, ready_tx).await {
            tracing::error!(%error, "the session manager stopped");
        }
    });

    let mut server_task = tokio::spawn(server::serve(listener, log.clone(), tokens, provider));
    let ready = tokio::select! {
        ready = ready_rx => ready,
        // The server can only end here if serving failed; `bind` already proved
        // the socket is ours, so this is a genuine error rather than a
        // competitor, and no session has been launched yet.
        result = &mut server_task => {
            shutdown_tx.send(true).ok();
            manager_task.abort();
            return match result {
                Ok(result) => result.map_err(RunError::Serve),
                Err(error) => Err(RunError::Task(error.to_string())),
            };
        },
    };
    if ready.is_err() {
        // The manager stopped before signalling; the kickoff would go nowhere.
        shutdown_tx.send(true).ok();
        manager_task.abort();
        server_task.abort();
        return Err(RunError::Task(String::from(
            "the session manager stopped before becoming ready",
        )));
    }
    // Subscribe before the kickoff so a fast announcement is not missed.
    let mut events = log.subscribe();
    if let Err(error) = log
        .publish(Event::new(
            agentd::session::SESSION_REQUESTED,
            serde_json::json!({ "session_id": args.session_id }),
        ))
        .await
    {
        // The kickoff is the whole point of `up`; a failed durable append means
        // the log is unusable, so it is reported rather than silently serving a
        // daemon with no agent.
        shutdown_tx.send(true).ok();
        manager_task.abort();
        server_task.abort();
        return Err(RunError::Log(error));
    }

    // The server keeps running while the announcement is waited for, so the
    // agent's connection is served.
    let conversation = agentd::up::await_conversation(
        &mut events,
        &layout.workdir.to_string_lossy(),
        &args.session_id,
    )
    .await;
    // The path is named regardless, because it is the one `agentd init` writes
    // and `agentd-publish` would default to anyway; when it is absent the
    // command cannot work, so say which file `init` must create.
    let user_token = agentd_events::paths::default_user_token();
    if !user_token.exists() {
        tracing::warn!(
            path = %user_token.display(),
            "the user token is missing; run `agentd init` in this config directory \
             before using the printed command",
        );
    }
    // A delimited block, so the copy-pasteable command is still findable among
    // the JSON tracing lines that share this stream.
    println!("--- prompt this session with ---");
    println!("{}", layout.publish_command(conversation.as_deref()));
    println!("--------------------------------");

    // The server runs until a signal stops it; `up` returns when it does and
    // then asks the manager to stop.
    let result = match server_task.await {
        Ok(result) => result.map_err(RunError::Serve),
        Err(error) => Err(RunError::Task(error.to_string())),
    };
    shutdown_tx.send(true).ok();
    let _ = manager_task.await;
    result
}

/// The agent id recorded on `session.*` events for a one-shot launch.
#[cfg(feature = "sandbox")]
const SESSION_AGENT_ID: &str = "urn:mokmokd:session";

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
    #[cfg(feature = "sandbox")]
    {
        // The generated template has no `default_model`, so the model is named
        // here; without one `up` refuses to start. The template's single
        // provider receives a bare name unchanged, so any name resolves until
        // the operator edits the file for their own server.
        println!(
            "or run the daemon and a sandboxed agent in one shot with:\n  \
             agentd up --workdir <workspace> --model <model>"
        );
        println!(
            "  (edit {} first: it names a local server and no model)",
            initialized.providers_path.display(),
        );
    }
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let telemetry = match agentd::telemetry::init() {
        Ok(telemetry) => telemetry,
        Err(error) => {
            eprintln!("{error}");
            return std::process::ExitCode::FAILURE;
        },
    };

    let code = match run().await {
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

#[cfg(test)]
mod provider_tests {
    #[cfg(feature = "sandbox")]
    use super::RunError;
    use super::load_provider;

    /// Writes a provider config with two providers, so a bare model name is
    /// ambiguous and cannot resolve by the sole-provider rule.
    fn write_config(
        dir: &std::path::Path,
        body: &str,
    ) -> std::path::PathBuf {
        let path = dir.join("providers.json");
        std::fs::write(&path, body).expect("config");
        path
    }

    const TWO_PROVIDERS: &str = r#"{
      "providers": {
        "a": { "kind": "open_ai_compatible", "base_url": "http://127.0.0.1:1/v1" },
        "b": { "kind": "open_ai_compatible", "base_url": "http://127.0.0.1:2/v1" }
      },
      "models": {},
      "default_model": null
    }"#;

    /// The two-provider config with the alias `fast` routed to `a`.
    #[cfg(feature = "sandbox")]
    fn with_fast_alias() -> String {
        TWO_PROVIDERS.replace(
            r#""models": {}"#,
            r#""models": { "fast": { "provider": "a", "model": "m" } }"#,
        )
    }

    #[cfg(feature = "sandbox")]
    #[test]
    fn a_default_model_naming_no_alias_fails_with_the_config_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_config(
            dir.path(),
            &TWO_PROVIDERS.replace(r#""default_model": null"#, r#""default_model": "ghost""#),
        );

        let error = super::load_provider_for(Some(&path), None)
            .err()
            .expect("an unresolvable default model must fail");

        let RunError::Model {
            reason,
            path: named,
        } = error
        else {
            unreachable!("expected a model error");
        };
        assert!(reason.contains("ghost"), "{reason}");
        assert_eq!(named, path);
    }

    #[cfg(feature = "sandbox")]
    #[test]
    fn a_model_that_resolves_through_an_alias_is_accepted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_config(dir.path(), &with_fast_alias());

        assert!(super::load_provider_for(Some(&path), Some("fast")).is_ok());
    }

    /// `serve` resolves the model per request, so it must accept a config for
    /// which no bare model resolves.
    #[test]
    fn verification_is_off_for_serve() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_config(dir.path(), TWO_PROVIDERS);

        assert!(load_provider(Some(&path)).is_ok());
    }
}
