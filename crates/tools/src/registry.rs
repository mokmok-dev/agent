//! The table every other surface reads.

use std::fmt;

use agent_core::event::Risk;
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::Value;

use crate::ctx::Ctx;
use crate::envelope::{failure, success};
use crate::error::ToolError;
use crate::tools::{EditFile, ListDir, ReadFile, Run, Search, WriteFile};
use crate::{Class, Code, Limits, Programs, Scope, Tool};

/// One tool: its declaration, its schema, and the function that runs it.
#[derive(Debug, Clone, Copy)]
pub struct RegisteredTool {
    pub name: &'static str,
    pub description: &'static str,
    pub class: Class,
    pub risk: Risk,
    pub limits: Limits,
    pub scope: Scope,
    pub env: &'static [&'static str],
    pub programs: Programs,
    /// The function schema the provider receives.
    pub parameters: fn() -> Value,
    /// One call: the model's bytes in, one envelope out.
    pub call: fn(&[u8], &Ctx<'_>) -> Vec<u8>,
}

/// Why the names given to [`select`] were not usable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectError {
    /// No tool holds the name.
    Unknown(String),
}

impl fmt::Display for SelectError {
    fn fmt(
        &self,
        formatter: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        match self {
            Self::Unknown(name) => write!(formatter, "there is no tool named {name}"),
        }
    }
}

impl std::error::Error for SelectError {}

static TOOLS: [RegisteredTool; 6] = [
    register::<EditFile>(Programs::None),
    register::<ListDir>(Programs::None),
    register::<ReadFile>(Programs::None),
    register::<Run>(Programs::Listed),
    register::<Search>(Programs::None),
    register::<WriteFile>(Programs::None),
];

const fn register<T: Tool>(programs: Programs) -> RegisteredTool {
    RegisteredTool {
        name: T::NAME,
        description: T::DESCRIPTION,
        class: T::CLASS,
        risk: T::RISK,
        limits: T::LIMITS,
        scope: T::SCOPE,
        env: T::ENV,
        programs,
        parameters: parameters::<T::Args>,
        call: call::<T>,
    }
}

/// Every tool this build ships, in name order.
#[must_use]
pub const fn registry() -> &'static [RegisteredTool] {
    &TOOLS
}

/// The tools the names ask for, or all of them.
///
/// # Errors
///
/// Returns [`SelectError::Unknown`] for a name no tool holds.
pub fn select(names: Option<&[String]>) -> Result<Vec<&'static RegisteredTool>, SelectError> {
    let Some(names) = names else {
        return Ok(TOOLS.iter().collect());
    };
    names
        .iter()
        .map(|name| {
            TOOLS
                .iter()
                .find(|tool| tool.name == name)
                .ok_or_else(|| SelectError::Unknown(name.clone()))
        })
        .collect()
}

/// The exact `tools` array the provider receives.
#[derive(Serialize)]
struct BlockEntry {
    #[serde(rename = "type")]
    kind: &'static str,
    function: Function,
}

#[derive(Serialize)]
struct Function {
    name: &'static str,
    description: &'static str,
    parameters: Value,
}

/// The provider's tools array: compact, sorted by name, and made only of the
/// registry's own facts.
#[must_use]
pub fn tool_block(tools: &[&RegisteredTool]) -> String {
    let mut sorted = tools.to_vec();
    sorted.sort_unstable_by_key(|tool| tool.name);
    let block: Vec<BlockEntry> = sorted
        .into_iter()
        .map(|tool| BlockEntry {
            kind: "function",
            function: Function {
                name: tool.name,
                description: tool.description,
                parameters: (tool.parameters)(),
            },
        })
        .collect();
    serde_json::to_string(&block).unwrap_or_else(|_| String::from("[]"))
}

/// The schema of one tool's arguments, without the root keywords providers
/// reject.
fn parameters<A: JsonSchema>() -> Value {
    let schema = serde_json::to_value(schemars::schema_for!(A));
    let mut schema = schema.unwrap_or_else(|_| serde_json::json!({ "type": "object" }));
    if let Some(object) = schema.as_object_mut() {
        object.remove("$schema");
        object.remove("title");
    }
    schema
}

fn call<T: Tool>(
    input: &[u8],
    ctx: &Ctx<'_>,
) -> Vec<u8> {
    let outcome = match serde_json::from_slice::<T::Args>(input) {
        Ok(args) => T::run(args, ctx),
        Err(error) => Err(ToolError::new(
            Code::InvalidArgs,
            format!("{}: {error}", T::NAME),
        )),
    };
    let bytes = match outcome {
        Ok(output) => success(&output),
        Err(error) => failure(&error),
    };
    if bytes.len() > T::LIMITS.event_bytes {
        return failure(&ToolError::too_large(format!(
            "{} answered with {} bytes, over its {} byte cap",
            T::NAME,
            bytes.len(),
            T::LIMITS.event_bytes
        )));
    }
    bytes
}

#[cfg(test)]
mod tests;
