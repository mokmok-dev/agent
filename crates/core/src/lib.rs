//! Pure domain model for the agent. No clock, no files, no sockets.
//!
//! The crate is `no_std` so that the constraint is checked by the compiler
//! rather than by review. Hashing is the only dependency that touches bytes,
//! and the canonical writer is the single definition of an event's identity.

#![no_std]

extern crate alloc;

#[cfg(test)]
extern crate std;

pub mod canonical;
pub mod decide;
pub mod event;
pub mod fold;
pub mod fork;
pub mod hash;
pub mod ids;
pub mod taxonomy;
pub mod time;
