//! The 24/7 agent.
//!
//! The first implemented subsystem is the event bus write-ahead log
//! ([`wal`]). See `docs/event-bus/` for its design.
//!
//! The event envelope is modeled in [`cloudevent`], the in-process fan-out in
//! [`broker`], the UDS/WebSocket transport in [`transport`], the wire messages
//! in [`protocol`], durable subscriber progress in [`cursor`], the wired state
//! machine in [`bus`], the authority claim in [`authority`], and the async
//! connection loop in [`server`].

pub mod authority;
pub mod broker;
pub mod bus;
pub mod cloudevent;
pub mod cursor;
pub mod protocol;
#[cfg(unix)]
pub mod server;
#[cfg(unix)]
pub mod transport;
pub mod wal;
