//! The errors the client server returns.

use std::net::SocketAddr;
use std::path::PathBuf;
use thiserror::Error;

/// Errors returned when binding or serving the client server.
///
/// The name follows the workspace's `<Noun>Error` convention and the sibling
/// that plays the same role for the daemon (`agentd::server::ServerError`).
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ServerError {
    /// Reading a bearer token file failed.
    #[error("reading the token file {} failed", path.display())]
    Token {
        /// The token file that could not be read.
        path: PathBuf,
        /// The underlying failure.
        #[source]
        source: std::io::Error,
    },
    /// The requested bind address is not loopback.
    #[error("{0} is not a loopback address, and the client server has no authentication")]
    NotLoopback(SocketAddr),
    /// Binding the downstream listener failed.
    #[error("binding {address} failed")]
    Bind {
        /// The address that could not be bound.
        address: SocketAddr,
        /// The underlying failure.
        #[source]
        source: std::io::Error,
    },
    /// Serving the downstream listener failed.
    #[error(transparent)]
    Serve(#[from] std::io::Error),
}
