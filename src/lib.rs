//! The 24/7 agent.
//!
//! The first implemented subsystem is the event bus write-ahead log
//! ([`wal`]). See `docs/event-bus/` for its design.

pub mod wal;
