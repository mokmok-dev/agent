//! The errors the client server returns.

use std::net::SocketAddr;
use std::path::PathBuf;
use thiserror::Error;

/// Errors returned when starting or running the client server.
#[derive(Debug, Error)]
pub enum RunError {
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
    #[error(
        "--bind {0} is not a loopback address, and the client server has no \
         authentication"
    )]
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
