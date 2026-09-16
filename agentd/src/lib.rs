//! Daemon components for the choreography-style event flow.
//!
//! The binary composes [`server`] with the durable [`agentd_events::EventLog`]
//! into a running daemon; the module is exposed so the same wiring can be
//! embedded elsewhere and exercised by integration tests.

pub mod server;
