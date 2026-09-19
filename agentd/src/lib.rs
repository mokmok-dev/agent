//! Daemon components for the choreography-style event flow.
//!
//! The binary composes [`server`] with the durable [`agentd_events::EventLog`]
//! into a running daemon; the module is exposed so the same wiring can be
//! embedded elsewhere and exercised by integration tests.
//!
//! Behind the `sandbox` feature, [`session`] supervises sandboxed child
//! processes and [`bridge`] converts a third-party tool's stdio protocol to and
//! from events, so a tool that does not speak `CloudEvents` still takes part in
//! the choreography.

pub mod auth;
#[cfg(feature = "sandbox")]
pub mod bridge;
pub mod init;
pub mod server;
#[cfg(feature = "sandbox")]
pub mod session;
