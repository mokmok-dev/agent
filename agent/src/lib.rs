//! The 24/7 agent.
//!
//! The first implemented subsystem is the event bus write-ahead log
//! ([`wal`]). See `docs/event-bus/` for its design.
//!
//! The event envelope is modeled in [`cloudevent`].

pub mod cloudevent;
pub mod wal;
