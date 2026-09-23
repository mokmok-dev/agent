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

mod cli;
mod error;
mod frame;
mod policy;
mod server;
mod uplink;

pub use cli::{Args, Config, DEFAULT_BIND};
pub use error::RunError;

use cli::read_token;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::watch;

/// Binds the downstream listener at `address`.
///
/// Returning the bound listener lets a caller learn the port when it passed `0`,
/// and lets an embedder serve the same state on its own listener.
///
/// # Errors
///
/// Returns [`RunError::Bind`] if `address` cannot be bound.
pub async fn bind(address: SocketAddr) -> Result<TcpListener, RunError> {
    TcpListener::bind(address)
        .await
        .map_err(|source| RunError::Bind { address, source })
}

/// Serves `listener` until `shutdown` becomes `true`.
///
/// Reads the bearer tokens, starts the publish task, and relays the daemon's
/// event API to every downstream client.
///
/// # Errors
///
/// Returns [`RunError::Token`] if a token file cannot be read, and
/// [`RunError::Serve`] if serving the listener fails.
pub async fn serve(
    listener: TcpListener,
    config: &Config,
    shutdown: watch::Receiver<bool>,
) -> Result<(), RunError> {
    let user_token = Arc::new(read_token(&config.token_file)?);
    let admin_token = config
        .admin_token_file
        .as_ref()
        .map(|path| read_token(path))
        .transpose()?
        .map(Arc::new);
    let (uplink_tx, uplink_rx) = uplink::channel();
    let uplink = uplink::Uplink::new(
        config.socket.clone(),
        Arc::clone(&user_token),
        admin_token,
        config.publish_timeout,
    );
    tokio::spawn(uplink::run(uplink, uplink_rx, shutdown.clone()));
    let state = Arc::new(server::AppState {
        socket: config.socket.clone(),
        user_token,
        allow_approve: config.allow_approve,
        uplink: uplink_tx,
    });
    server::serve(listener, state, shutdown).await
}

/// Binds the configured address, reports it, and serves until a signal.
///
/// # Errors
///
/// As [`bind`] and [`serve`].
pub async fn run(config: Config) -> Result<(), RunError> {
    let listener = bind(config.bind).await?;
    match listener.local_addr() {
        Ok(address) => println!("agentd-client listening on {address}"),
        Err(error) => tracing::warn!(%error, "the bound address could not be read"),
    }
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    tokio::spawn(watch_for_signal(shutdown_tx));
    serve(listener, &config, shutdown_rx).await
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
