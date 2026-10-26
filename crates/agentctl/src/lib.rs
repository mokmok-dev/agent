//! The operator CLI. `clap` reads the arguments, this crate decides the rest.
//!
//! [Decision 25](../../../docs/spec/architecture/decisions.md) gives `agentctl`
//! a framework and `agentd` its own parser. The split is the point: the command
//! declaration here is `clap`'s, and everything the interface spec pins down,
//! the exit table and the error object, stays in [`exit`] where a test can
//! reach it.

pub mod cli;
pub mod exit;
pub mod report;

pub use cli::{ForkPoint, Intent, Invocation, Parsed, Supervisor, parse};
pub use exit::{Class, Exit, Failure};
