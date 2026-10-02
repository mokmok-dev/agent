//! The purpose-built agent: the program that runs inside a session.
//!
//! The agent is a bus peer. It subscribes to its session's input, does what the
//! command asks, and publishes what it did. The daemon starts and stops it; the
//! bus is its whole interface. See `docs/session/agent.md`.
//!
//! Milestone 5 is the **loop and the contract**: connect, subscribe, react to a
//! command, publish output, and acknowledge progress so nothing is lost across a
//! restart. Milestone 6 adds the capabilities that decide what a command does:
//! [`coding`], which reads, searches, patches, and runs commands in the session's
//! workspace, and [`task`], which drives a model through [`model::Model`] with the
//! coding belt as its tools.

mod child;
pub mod coding;
pub mod contract;
mod diff;
pub mod loopcore;
pub mod model;
pub mod openai;
pub mod shell;
pub mod task;

pub use coding::{
    ACTION_CODE_SEARCH as CODE_SEARCH_ACTION, ACTION_PATCH as PATCH_ACTION,
    ACTION_READ as READ_ACTION, Coding, Mode, UnknownMode,
};
pub use contract::{Command, Output, OutputKind};
pub use loopcore::{AgentConfig, Error};
pub use model::{Message, Model, Response, Tool, ToolCall};
pub use shell::{ACTION as SHELL_ACTION, Shell};
pub use task::{ACTION as TASK_ACTION, Task};
