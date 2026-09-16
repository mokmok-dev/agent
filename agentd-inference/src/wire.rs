//! The provider-neutral inference wire types.
//!
//! The shapes mirror the streaming completion protocol most providers expose
//! (roles, messages with tool calls, streaming deltas with a terminal reason),
//! so a provider adapter is mostly a field mapping rather than a translation
//! layer.

use serde::{Deserialize, Serialize};

/// The author of a [`Message`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Instructions the model sees before the conversation.
    System,
    /// A prompt from the user (in `agentd`, an `agent.inbox` event).
    User,
    /// A reply from the model.
    Assistant,
    /// The result of a tool the model asked to run.
    Tool,
}

/// A tool call the model requested.
///
/// `arguments` is the provider's JSON-encoded argument object, kept as a string
/// so the contract does not depend on the tool's schema.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    /// The provider's identifier for this call, echoed on the tool result.
    pub id: String,
    /// The name of the tool to run.
    pub name: String,
    /// The JSON-encoded arguments.
    pub arguments: String,
}

/// One turn of a conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    /// The author.
    pub role: Role,
    /// The text content; empty for an assistant turn that only calls tools.
    pub content: String,
    /// Tool calls the assistant requested in this turn.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// For a [`Role::Tool`] message, the call it answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl Message {
    /// A system instruction.
    #[must_use]
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    /// A user prompt.
    #[must_use]
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    /// An assistant reply that calls no tools.
    #[must_use]
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    /// An assistant reply that requests `tool_calls`.
    #[must_use]
    pub fn with_tool_calls(
        content: impl Into<String>,
        tool_calls: Vec<ToolCall>,
    ) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            tool_calls,
            tool_call_id: None,
        }
    }

    /// The result of running the tool call `tool_call_id`.
    #[must_use]
    pub fn tool(
        tool_call_id: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: Some(tool_call_id.into()),
        }
    }
}

/// A tool the model may call, described in JSON Schema.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSpec {
    /// The name the model uses to call the tool.
    pub name: String,
    /// A description that tells the model when to use it.
    pub description: String,
    /// The JSON Schema of the arguments.
    pub parameters: serde_json::Value,
}

/// One inference request from a node to the daemon.
///
/// `Default` has an empty conversation and no tools, so callers that need only
/// one of the two fields can use struct-update syntax.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct InferenceRequest {
    /// The conversation so far, oldest first, including the system instruction.
    pub messages: Vec<Message>,
    /// The tools the model may call.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolSpec>,
}

/// One streamed piece of an inference response.
///
/// A stream ends at the first [`Delta::Done`] or [`Delta::Error`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Delta {
    /// An increment of the response text.
    Text {
        /// The text fragment.
        text: String,
    },
    /// A complete tool call the model requested.
    ToolCall {
        /// The provider's identifier for the call.
        id: String,
        /// The tool to run.
        name: String,
        /// The JSON-encoded arguments.
        arguments: String,
    },
    /// The response finished normally.
    Done {
        /// The provider's stop reason, if it reported one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        finish_reason: Option<String>,
    },
    /// The response failed; the message is a human-readable reason.
    Error {
        /// The failure reason.
        message: String,
    },
}

impl Delta {
    /// Whether this delta terminates the stream.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::Done { .. } | Self::Error { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::{Delta, InferenceRequest, Message, Role, ToolCall, ToolSpec};
    use serde_json::json;

    #[test]
    fn messages_round_trip_through_json() {
        let request = InferenceRequest {
            messages: vec![
                Message::system("be helpful"),
                Message::user("list files"),
                Message::with_tool_calls(
                    "",
                    vec![ToolCall {
                        id: String::from("call-1"),
                        name: String::from("shell"),
                        arguments: String::from(r#"{"command":"ls"}"#),
                    }],
                ),
                Message::tool("call-1", "exit code: 0\nsrc"),
            ],
            tools: vec![ToolSpec {
                name: String::from("shell"),
                description: String::from("run a shell command"),
                parameters: json!({ "type": "object" }),
            }],
        };

        let encoded = serde_json::to_string(&request).expect("request should serialize");
        let decoded: InferenceRequest =
            serde_json::from_str(&encoded).expect("request should deserialize");

        assert_eq!(decoded, request);
        assert_eq!(decoded.messages[0].role, Role::System);
        assert_eq!(decoded.messages[3].tool_call_id.as_deref(), Some("call-1"));
    }

    #[test]
    fn an_assistant_text_message_omits_the_tool_fields() {
        let encoded =
            serde_json::to_value(Message::assistant("hello")).expect("message should serialize");

        assert_eq!(encoded, json!({ "role": "assistant", "content": "hello" }));
    }

    #[test]
    fn deltas_are_tagged_and_terminal_detection_is_explicit() {
        let text: Delta =
            serde_json::from_str(r#"{"type":"text","text":"hi"}"#).expect("text should decode");
        let done: Delta = serde_json::from_str(r#"{"type":"done"}"#).expect("done should decode");
        let error: Delta = serde_json::from_str(r#"{"type":"error","message":"boom"}"#)
            .expect("error should decode");

        assert_eq!(
            text,
            Delta::Text {
                text: String::from("hi")
            }
        );
        assert_eq!(
            done,
            Delta::Done {
                finish_reason: None
            }
        );
        assert_eq!(
            error,
            Delta::Error {
                message: String::from("boom")
            }
        );
        assert!(!text.is_terminal());
        assert!(done.is_terminal());
        assert!(error.is_terminal());
    }
}
