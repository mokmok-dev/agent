//! The purpose-built agent: the program that runs inside a session.
//!
//! The agent is a bus peer. It subscribes to its session's input, does what the
//! command asks, and publishes what it did. The daemon starts and stops it; the
//! bus is its whole interface. See `docs/session/agent.md`.
//!
//! Milestone 5 is the **loop and the contract**: connect, subscribe, react to a
//! command, publish output, and acknowledge progress so nothing is lost across a
//! restart. Milestone 6 adds the capabilities that decide what a command does:
//! [`shell`], which runs an argv in the session's workspace, and [`task`], which
//! drives a model through [`model::Model`] with `shell` as its one tool.

pub mod contract;
pub mod loopcore;
pub mod model;
pub mod openai;
pub mod shell;
pub mod task;

pub use contract::{Command, Output, OutputKind};
pub use loopcore::{AgentConfig, Error};
pub use model::{Message, Model, Response, Tool, ToolCall};
pub use shell::{ACTION as SHELL_ACTION, Shell};
pub use task::{ACTION as TASK_ACTION, Task};
