//! `agent`: run the durable event bus.
//!
//! Binds the transport listener(s), opens the bus over `--dir`, and serves the
//! wire protocol until interrupted. This is the daemon the rest of `docs/`
//! assumes; it is also the subject the observability milestone observes.
//!
//! Access control is deliberate and fails closed: a peer connects only if its
//! UID or GID is on the allowlist, and a privileged event type is publishable
//! only from the authority listener. With no `--authority-socket`, no connection
//! holds authority, so the sandbox decision events cannot be published at all.

use std::path::PathBuf;
use std::process::ExitCode;

use agent::broker::DEFAULT_QUEUE_CAPACITY;
use agent::bus::Bus;
use agent::server::Server;
use agent::transport::{Allowlist, Listener, WebSocketConfig};
use clap::{Parser, ValueEnum};
use tracing_subscriber::EnvFilter;

/// Run the durable event bus over a Unix domain socket.
#[derive(Debug, Parser)]
#[command(name = "agent", version, about)]
struct Cli {
    /// Directory for the write-ahead log and the durable cursors.
    #[arg(long)]
    dir: PathBuf,

    /// Path of the UDS to listen on for ordinary connections.
    #[arg(long)]
    socket: PathBuf,

    /// Path of a second UDS whose connections hold the authority claim. Omit to
    /// grant authority to nobody, which is the safe default.
    #[arg(long)]
    authority_socket: Option<PathBuf>,

    /// Permit a peer with this UID. Repeatable. With no UID or GID, nobody may
    /// connect.
    #[arg(long = "allow-uid")]
    allow_uids: Vec<u32>,

    /// Permit a peer with this GID. Repeatable.
    #[arg(long = "allow-gid")]
    allow_gids: Vec<u32>,

    /// Per-subscriber queue capacity, in events.
    #[arg(long, default_value_t = DEFAULT_QUEUE_CAPACITY)]
    capacity: usize,

    /// Log output format.
    #[arg(long, value_enum, default_value_t = LogFormat::Full)]
    log_format: LogFormat,
}

/// How to render logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum LogFormat {
    /// Human-readable, multi-line.
    Full,
    /// Human-readable, one line per event.
    Compact,
    /// One JSON object per event.
    Json,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_tracing(cli.log_format);

    match run(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(%error, "the bus stopped");
            ExitCode::FAILURE
        },
    }
}

/// Install the global log subscriber.
///
/// The library only emits `tracing` events; the binary decides where they go.
/// `RUST_LOG` overrides the default level; without it, `info` and above.
fn init_tracing(format: LogFormat) {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("agent=info,warn"));
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    match format {
        LogFormat::Full => builder.init(),
        LogFormat::Compact => builder.compact().init(),
        LogFormat::Json => builder.json().init(),
    }
}

/// Build the allowlist, and warn when it admits nobody.
fn allowlist(cli: &Cli) -> Allowlist {
    let mut allowlist = Allowlist::new();
    for uid in &cli.allow_uids {
        allowlist = allowlist.allow_uid(*uid);
    }
    for gid in &cli.allow_gids {
        allowlist = allowlist.allow_gid(*gid);
    }
    if allowlist.is_empty() {
        tracing::warn!(
            "the allowlist is empty: no peer may connect. Pass --allow-uid or --allow-gid.",
        );
    }
    allowlist
}

/// Bind the listeners and run the server.
///
/// All setup runs inside the runtime: `Listener::bind` creates a tokio socket,
/// which needs a reactor, and the multi-thread runtime is also what keeps one
/// connection's blocking `fsync` from stalling the others.
fn run(cli: &Cli) -> Result<(), Box<dyn std::error::Error>> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(serve(cli))
}

/// The async body of [`run`].
async fn serve(cli: &Cli) -> Result<(), Box<dyn std::error::Error>> {
    let config = WebSocketConfig::default();
    let listener = Listener::bind(&cli.socket, config)?;
    tracing::info!(socket = %cli.socket.display(), "listening");

    let authority_listener = cli
        .authority_socket
        .as_ref()
        .map(|path| {
            let listener = Listener::bind(path, config)?;
            tracing::info!(
                socket = %path.display(),
                "listening for authority connections",
            );
            Ok::<_, std::io::Error>(listener)
        })
        .transpose()?;
    if authority_listener.is_none() {
        tracing::warn!("no --authority-socket: no connection may publish a privileged event",);
    }

    let bus = Bus::open(&cli.dir, cli.capacity)?;
    tracing::info!(dir = %cli.dir.display(), head_seq = ?bus.head_seq(), "opened the bus");

    let server = Server::with_authority_listener(listener, authority_listener, bus, allowlist(cli));

    server.run().await?;
    Ok(())
}
