//! Daemon components for the choreography-style event flow.
//!
//! The binary composes [`server`] and [`eventstore`] into a running daemon;
//! the modules are exposed so the same wiring can be embedded elsewhere and
//! exercised by integration tests.

pub mod eventstore;
pub mod server;
