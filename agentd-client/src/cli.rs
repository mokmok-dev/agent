//! The client server's command line and the settings resolved from it.

use crate::error::ServerError;
use agentd_events::paths;
use clap::Parser;
use secrecy::SecretString;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The default downstream address.
///
/// Loopback only: the downstream socket carries no authentication, and
/// [`bind`](crate::bind) refuses anything else.
pub const DEFAULT_BIND: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8787);

/// The default seconds to wait for a publish verdict.
pub const DEFAULT_PUBLISH_TIMEOUT_SECS: u64 = 10;

/// The client server's arguments.
#[derive(Debug, Parser)]
#[command(name = "agentd-client", version = env!("CARGO_PKG_VERSION"))]
pub struct Args {
    /// The daemon's event WebSocket Unix socket.
    #[arg(long, default_value_os_t = paths::default_socket())]
    pub socket: PathBuf,
    /// A file whose entire contents is the bearer token used to read the log and
    /// to publish non-reserved events. Defaults to
    /// `$XDG_CONFIG_HOME/agentd/user.token`.
    #[arg(long, default_value_os_t = paths::default_user_token())]
    pub token_file: PathBuf,
    /// A file whose entire contents is the `authority` bearer token used to
    /// publish decisions. Defaults to `$XDG_CONFIG_HOME/agentd/admin.token`,
    /// and is only read with `--allow-approve`.
    #[arg(long)]
    pub admin_token_file: Option<PathBuf>,
    /// Forward a downstream client's permission decisions on the authority
    /// connection. Without it every reserved event type is refused locally, so
    /// the daemon is never asked to append one.
    #[arg(long)]
    pub allow_approve: bool,
    /// The TCP address to serve on. Only a loopback address is accepted.
    #[arg(long, default_value_t = DEFAULT_BIND)]
    pub bind: SocketAddr,
    /// A browser `Origin` allowed to connect, repeated per origin. A browser
    /// always sends `Origin` and a TUI does not, so the default — no origin —
    /// refuses every page while leaving a non-browser client alone.
    #[arg(long = "allow-origin", value_name = "ORIGIN")]
    pub allow_origin: Vec<String>,
    /// Wall-clock seconds to wait for a publish verdict before reporting the
    /// outcome as unknown.
    #[arg(long, default_value_t = DEFAULT_PUBLISH_TIMEOUT_SECS)]
    pub publish_timeout_secs: u64,
}

/// The settings the client server runs with.
///
/// No bearer secret is a field: [`serve`](crate::serve) reads the token files
/// itself, so the configuration can be logged without redaction.
#[derive(Debug, Clone)]
pub struct Config {
    /// The TCP address to serve on. It must be loopback: the downstream socket
    /// carries no authentication, so [`bind`](crate::bind) rejects any other
    /// address rather than exposing publishing to the network.
    pub bind: SocketAddr,
    /// The daemon's event WebSocket Unix socket.
    pub socket: PathBuf,
    /// The bearer token file for reading and publishing.
    pub token_file: PathBuf,
    /// The `authority` bearer token file. It is read only when
    /// [`allow_approve`](Config::allow_approve) is set; an embedder may leave it
    /// set while approvals are off, and then it is ignored.
    pub admin_token_file: Option<PathBuf>,
    /// Whether decisions are forwarded on the authority connection.
    pub allow_approve: bool,
    /// The browser origins allowed to connect. Empty refuses every request that
    /// carries an `Origin` header, which is every browser request, while leaving
    /// a non-browser client alone.
    pub allow_origins: Vec<String>,
    /// How long to wait for a publish verdict.
    pub publish_timeout: Duration,
}

impl Default for Config {
    /// The XDG paths and the default address, as the command line resolves them.
    fn default() -> Self {
        Self {
            bind: DEFAULT_BIND,
            socket: paths::default_socket(),
            token_file: paths::default_user_token(),
            admin_token_file: None,
            allow_approve: false,
            allow_origins: Vec::new(),
            publish_timeout: Duration::from_secs(DEFAULT_PUBLISH_TIMEOUT_SECS),
        }
    }
}

impl TryFrom<&Args> for Config {
    type Error = ServerError;

    /// Validates the arguments and resolves the paths they imply.
    ///
    /// # Errors
    ///
    /// Returns [`ServerError::NotLoopback`] for a non-loopback `--bind`, because
    /// the downstream socket carries no authentication. [`bind`](crate::bind)
    /// enforces the same rule, so an embedder that skips the command line cannot
    /// bypass it.
    fn try_from(args: &Args) -> Result<Self, Self::Error> {
        if !args.bind.ip().is_loopback() {
            return Err(ServerError::NotLoopback(args.bind));
        }
        if args.admin_token_file.is_some() && !args.allow_approve {
            tracing::warn!("--admin-token-file is ignored without --allow-approve");
        }
        let admin_token_file = args.allow_approve.then(|| {
            args.admin_token_file
                .clone()
                .unwrap_or_else(|| paths::config_dir().join("admin.token"))
        });
        Ok(Self {
            bind: args.bind,
            socket: args.socket.clone(),
            token_file: args.token_file.clone(),
            admin_token_file,
            allow_approve: args.allow_approve,
            allow_origins: args.allow_origin.clone(),
            publish_timeout: Duration::from_secs(args.publish_timeout_secs),
        })
    }
}

/// A token file's contents, trimmed of the trailing newline a file carries.
///
/// # Errors
///
/// Returns [`ServerError::Token`] when the file cannot be read.
pub fn read_token(path: &Path) -> Result<SecretString, ServerError> {
    let token = std::fs::read_to_string(path).map_err(|source| ServerError::Token {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(SecretString::from(token.trim().to_owned()))
}
