//! The purpose-built agent: the program that runs inside a session.
//!
//! The agent is a bus peer. It subscribes to its session's input, does what the
//! command asks, and publishes what it did. The daemon starts and stops it; the
//! bus is its whole interface. See `docs/session/agent.md`.
//!
//! Milestone 5 is the **loop and the contract**: connect, subscribe, react to a
//! command, publish output, and acknowledge progress so nothing is lost across a
//! restart. What the agent does with a command is [`run`]'s handler, which a
//! later milestone extends into the coding agent.

pub mod contract;
pub mod loopcore;

pub use contract::{Command, Output, OutputKind};
pub use loopcore::{AgentConfig, Error};
