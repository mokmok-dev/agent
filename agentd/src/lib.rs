//! Daemon components for the choreography-style event flow.
//!
//! The binary composes [`server`] and [`log`] into a running daemon; the
//! modules are exposed so the same wiring can be embedded elsewhere and
//! exercised by integration tests. [`projection`] provides read models derived
//! from the log.

pub mod log;
pub mod projection;
pub mod server;