//! The `agent.*` event types the agent node publishes.
//!
//! They live here, beside the other shared contract constants, because both the
//! node that emits them and the daemon that supervises it (and prints a publish
//! command naming one) must agree on the exact strings. Only the node's
//! dependency on this crate is real, so this is the one place the contract can
//! be stated once.

/// A conversation began: its id, the workdir, and the model.
pub const AGENT_CONVERSATION_STARTED: &str = "agent.conversation.started";
/// A user prompt that starts a turn.
pub const AGENT_INBOX: &str = "agent.inbox";
/// A finalized assistant message.
pub const AGENT_MESSAGE: &str = "agent.message";
/// The result of a tool the assistant called.
pub const AGENT_TOOL_RESULT: &str = "agent.tool_result";
/// A unified diff the assistant applied, with the inverse that undoes it.
pub const AGENT_PATCH_APPLIED: &str = "agent.patch.applied";
/// A turn began.
pub const AGENT_TURN_STARTED: &str = "agent.turn.started";
/// A turn completed without a provider error.
pub const AGENT_TURN_COMPLETED: &str = "agent.turn.completed";
/// A turn failed, for example because the provider returned an error.
pub const AGENT_TURN_FAILED: &str = "agent.turn.failed";
