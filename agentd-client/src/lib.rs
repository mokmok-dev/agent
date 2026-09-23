//! The client server: the daemon's event API, reachable from a TUI or a browser.
//!
//! The daemon serves its event API on a Unix domain socket only, so a client
//! that is not in the daemon's process cannot reach it. This crate is the
//! out-of-process actor that relays that API over a loopback TCP WebSocket, so a
//! TUI or a browser can subscribe to the log, publish a prompt, and answer a
//! permission request without holding the daemon's bearer token.
//!
//! The server is deliberately stateless: it holds no database, journal, or read
//! model. The daemon's log is the source of truth, and every log frame it
//! forwards is the daemon's own, so a client that remembers the position of the
//! last event it applied can reconnect and resume. See
//! `docs/client-server.md` for the contract and the retry rules.
//!
//! [`run`] is the binary's entry: it binds [`Config::bind`], reports the address
//! on stdout, and stops on SIGINT or SIGTERM. [`bind`] and [`serve`] are the
//! composable pieces for an embedder that owns its own signals; `serve` takes a
//! shutdown receiver and never installs a handler.

mod cli;
mod error;
mod frame;
mod policy;
mod server;
mod uplink;

pub use cli::{Args, Config, DEFAULT_BIND, DEFAULT_PUBLISH_TIMEOUT_SECS};
pub use error::ServerError;

use cli::read_token;
use secrecy::SecretString;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::watch;

/// The bearer tokens a running server authenticates with.
struct Tokens {
    /// The token every downstream read connection and user publish uses.
    user: Arc<SecretString>,
    /// The `authority` token, present only when approvals are enabled.
    admin: Option<Arc<SecretString>>,
}

/// Reads the bearer tokens the configuration names.
///
/// The authority token is read only when approvals are enabled, so a
/// configuration that carries the path while the flag is off never touches the
/// file.
fn load_tokens(config: &Config) -> Result<Tokens, ServerError> {
    let user = Arc::new(read_token(&config.token_file)?);
    let admin = if config.allow_approve {
        config
            .admin_token_file
            .as_ref()
            .map(|path| read_token(path))
            .transpose()?
            .map(Arc::new)
    } else {
        None
    };
    Ok(Tokens { user, admin })
}

/// Binds the downstream listener at `address`.
///
/// Returning the bound listener lets a caller learn the port when it passed `0`,
/// and lets an embedder serve the same state on its own listener. Only a
/// loopback address is accepted, at this choke point as well as on the command
/// line, because the downstream socket carries no authentication.
///
/// # Errors
///
/// Returns [`ServerError::NotLoopback`] for an address that is not loopback, and
/// [`ServerError::Bind`] if `address` cannot be bound.
pub async fn bind(address: SocketAddr) -> Result<TcpListener, ServerError> {
    if !address.ip().is_loopback() {
        return Err(ServerError::NotLoopback(address));
    }
    TcpListener::bind(address)
        .await
        .map_err(|source| ServerError::Bind { address, source })
}

/// Serves `listener` until `shutdown` becomes `true`.
///
/// Reads the bearer tokens, starts the publish task, and relays the daemon's
/// event API to every downstream client. It never installs a signal handler, so
/// an embedder keeps control of shutdown.
///
/// # Errors
///
/// Returns [`ServerError::Token`] if a token file cannot be read, and
/// [`ServerError::Serve`] if serving the listener fails.
pub async fn serve(
    listener: TcpListener,
    config: &Config,
    shutdown: watch::Receiver<bool>,
) -> Result<(), ServerError> {
    let tokens = load_tokens(config)?;
    serve_with(listener, config, tokens, shutdown).await
}

/// Starts the publish task and serves the router with already-loaded tokens.
async fn serve_with(
    listener: TcpListener,
    config: &Config,
    tokens: Tokens,
    shutdown: watch::Receiver<bool>,
) -> Result<(), ServerError> {
    let (uplink_tx, uplink_rx) = uplink::channel();
    let uplink = uplink::Uplink::new(
        config.socket.clone(),
        Arc::clone(&tokens.user),
        tokens.admin,
        config.publish_timeout,
    );
    tokio::spawn(uplink::run(uplink, uplink_rx, shutdown.clone()));
    let state = Arc::new(server::AppState {
        socket: config.socket.clone(),
        user_token: tokens.user,
        allow_approve: config.allow_approve,
        allow_origins: config.allow_origins.clone(),
        uplink: uplink_tx,
        shutdown: shutdown.clone(),
    });
    server::serve(listener, state, shutdown).await
}

/// Binds the configured address, reports it, and serves until a signal.
///
/// The tokens are read before the address is reported, so a configuration that
/// cannot serve never prints that it is listening.
///
/// # Errors
///
/// As [`bind`] and [`serve`].
pub async fn run(config: Config) -> Result<(), ServerError> {
    let listener = bind(config.bind).await?;
    let tokens = load_tokens(&config)?;
    match listener.local_addr() {
        Ok(address) => println!("agentd-client listening on {address}"),
        Err(error) => tracing::warn!(%error, "the bound address could not be read"),
    }
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    tokio::spawn(watch_for_signal(shutdown_tx));
    serve_with(listener, &config, tokens, shutdown_rx).await
}

/// Signals shutdown when the process receives SIGINT or SIGTERM.
#[cfg(unix)]
async fn watch_for_signal(shutdown: watch::Sender<bool>) {
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
    shutdown.send(true).ok();
}

/// Signals shutdown when the process receives an interrupt.
#[cfg(not(unix))]
async fn watch_for_signal(shutdown: watch::Sender<bool>) {
    if tokio::signal::ctrl_c().await.is_ok() {
        shutdown.send(true).ok();
    }
}
