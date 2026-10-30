//! The first-party tools.
//!
//! One table describes every tool this build ships: its name, its argument
//! schema, what it may touch, and the function that runs it. The binary
//! `agent-tool` is the only way a model reaches one. It reads one JSON object on
//! standard input and writes one JSON object on standard output, and it exits
//! zero for every outcome the model is meant to read.

use std::time::Duration;

use agent_core::event::Risk;
use schemars::JsonSchema;
use serde::Serialize;
use serde::de::DeserializeOwned;

mod allow;
mod atomic;
mod ctx;
mod envelope;
mod error;
mod path;
mod registry;
mod tools;

#[cfg(test)]
mod fixture;

pub use allow::{AllowError, Allowlist};
pub use ctx::Ctx;
pub use error::{Code, ToolError};
pub use path::Workspace;
pub use registry::{RegisteredTool, SelectError, registry, select, tool_block};

/// The longest a tool may run, and the cap an operator's override is held to.
pub const TIMEOUT_CAP_MS: u64 = 3_600_000;

/// Whether running a tool twice is the same as running it once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// Rerunning it after a crash converges on the same state.
    Idempotent,
    /// Rerunning it after a crash may apply the effect twice.
    NonIdempotent,
}

impl Class {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Idempotent => "idempotent",
            Self::NonIdempotent => "non-idempotent",
        }
    }
}

/// What a tool writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Writes nothing.
    Read,
    /// Writes inside the workspace.
    Workspace,
    /// Writes inside the workspace, the tool home, and `/tmp`.
    WorkspaceAndScratch,
}

/// The bounds one call runs under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// The wall clock bound on one call.
    pub timeout_ms: u64,
    /// The most bytes of serialized result one event may carry.
    pub event_bytes: usize,
}

impl Limits {
    /// The deadline as the executor takes it.
    #[must_use]
    pub const fn timeout(self) -> Duration {
        Duration::from_millis(self.timeout_ms)
    }
}

/// Which programs a tool may start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Programs {
    /// The tool starts no programs.
    None,
    /// The tool starts only the programs its own invocation lists.
    Listed,
}

/// One tool's declaration and its implementation.
pub trait Tool {
    /// The arguments the model writes.
    type Args: DeserializeOwned + JsonSchema;
    /// What the tool answers with.
    type Output: Serialize;

    /// The name the provider and the log use.
    const NAME: &'static str;
    /// The sentence the provider reads in the tool block.
    const DESCRIPTION: &'static str;
    /// What a crash between intent and result means for this tool.
    const CLASS: Class;
    /// What running it needs from the operator.
    const RISK: Risk;
    /// What it may write.
    const SCOPE: Scope;
    /// Its deadline and its event cap.
    const LIMITS: Limits;
    /// The environment variable names its process may see.
    const ENV: &'static [&'static str];

    /// Runs one call.
    ///
    /// # Errors
    ///
    /// Returns a [`ToolError`] the model is meant to read when the request
    /// cannot be carried out.
    fn run(
        args: Self::Args,
        ctx: &Ctx<'_>,
    ) -> Result<Self::Output, ToolError>;
}

#[cfg(test)]
mod tests {
    use super::Class;

    #[test]
    fn each_class_has_the_name_the_manifest_writes() {
        assert_eq!(Class::Idempotent.as_str(), "idempotent");
        assert_eq!(Class::NonIdempotent.as_str(), "non-idempotent");
    }
}
