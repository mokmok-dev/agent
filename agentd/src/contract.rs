//! The event contract between a session's clients and its agent.
//!
//! The agent owns `agent.session.<id>.output` and accepts
//! `agent.session.<id>.command` (`docs/session/agent.md`). A session's subject
//! is the id's subject, so both types carry the session as a `CloudEvents`
//! `subject` and the type names only the family. That keeps the contract one
//! table and the agent's filter one string comparison.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The event type carrying input for the agent.
pub const COMMAND: &str = "agent.session.command";
/// The event type the agent's progress and result are published under.
pub const OUTPUT: &str = "agent.session.output";

/// The payload of a [`COMMAND`] event.
///
/// `action` names what to do; `detail` carries the action's own arguments. The
/// agent ignores a command whose action it does not know, and reports that in its
/// output, so a client and an agent of different versions do not crash each
/// other.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Command {
    /// The action the agent should take.
    pub action: String,
    /// The action's own arguments, or `null` when it takes none.
    #[serde(default)]
    pub detail: Value,
}

/// The payload of an [`OUTPUT`] event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Output {
    /// What the agent did named in one word, so a consumer can route on it.
    pub kind: OutputKind,
    /// The action this output is about, echoed from the command.
    pub action: String,
    /// The action's result or the progress detail.
    #[serde(default)]
    pub detail: Value,
}

/// What an output event is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputKind {
    /// The agent started working on a command.
    Started,
    /// An action is in progress; `detail` carries the progress so far.
    Progress,
    /// The action finished.
    Done,
    /// The action failed, or the command was not understood.
    Error,
}

impl Command {
    /// Parse a command from an event's `data`.
    ///
    /// # Errors
    ///
    /// Returns [`ContractError::NotACommand`] when `data` does not parse.
    pub fn from_data(data: &Value) -> Result<Self, ContractError> {
        serde_json::from_value(data.clone()).map_err(ContractError::NotACommand)
    }
}

/// Failures of the contract.
#[derive(Debug, thiserror::Error)]
pub enum ContractError {
    /// An event's `data` did not parse as its type's payload.
    #[error("event data is not a valid payload: {0}")]
    NotACommand(#[source] serde_json::Error),
}

#[cfg(test)]
mod tests {
    // Tests for the contract: the payloads round-trip, and the agent tolerates a
    // command it does not understand.

    use serde_json::json;

    use super::*;

    #[test]
    fn a_command_round_trips() {
        let command = Command {
            action: "run".to_owned(),
            detail: json!({"script": "echo hi"}),
        };
        let parsed: Command =
            serde_json::from_value(json!({"action": "run", "detail": {"script": "echo hi"}}))
                .expect("parses");
        assert_eq!(parsed, command);
    }

    #[test]
    fn a_command_without_detail_parses() {
        let parsed: Command = serde_json::from_value(json!({"action": "stop"})).expect("parses");
        assert_eq!(parsed.action, "stop");
        assert!(parsed.detail.is_null());
    }

    #[test]
    fn an_unknown_field_is_rejected() {
        assert!(serde_json::from_value::<Command>(json!({"action": "a", "nope": 1})).is_err());
    }

    #[test]
    fn the_type_names_are_not_session_scoped() {
        // The session is carried by the CloudEvents `subject`, not by the type,
        // so the two families stay one table.
        assert_eq!(COMMAND, "agent.session.command");
        assert_eq!(OUTPUT, "agent.session.output");
    }
}
