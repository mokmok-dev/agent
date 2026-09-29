//! The model seam: what the agent asks a model, and what it gets back.
//!
//! A model-driven capability holds a [`Model`] rather than a provider's client,
//! so the loop that drives it is exercisable without a network. The types here
//! are the agent's own: a conversation of [`Message`]s and the [`Tool`]s it
//! offers. Nothing here is serialized, because a client owns the mapping to
//! whatever its provider speaks, and the agent's shape and a provider's do not
//! have to agree. The provider chosen for the first client is an
//! OpenAI-compatible `chat/completions` endpoint; its client does the flattening.
//!
//! **A client must bound what it reads.** A response is a whole assistant turn,
//! and reading it all into memory before checking its size is the mistake that
//! OOM-killed this agent once already. A client reads under a cap, the way the
//! `shell` capability bounds a command's output. Bounding after the fact is the
//! error, because by then the memory has been taken.

use serde_json::Value;

/// A model the agent can hold a conversation with.
pub trait Model: Send + Sync + std::fmt::Debug {
    /// Send the conversation so far and return the next assistant turn.
    ///
    /// # Errors
    ///
    /// Returns an [`Error`] when the model could not be reached or answered
    /// something unusable. The caller reports that and stops the task.
    fn complete(
        &self,
        messages: &[Message],
        tools: &[Tool],
    ) -> Result<Response, Error>;
}

/// One message in the conversation.
///
/// The variants are the four things a conversation contains, so a message cannot
/// carry a field its role has no meaning for.
#[derive(Debug, Clone)]
pub enum Message {
    /// What the agent tells the model about itself, before the work.
    System {
        /// The instruction.
        content: String,
    },
    /// The work to do.
    User {
        /// The task.
        content: String,
    },
    /// What the model said.
    Assistant {
        /// What it said in prose, which may be empty when it only calls tools.
        content: String,
        /// The tools it asked for. An empty list means it is finished.
        calls: Vec<ToolCall>,
    },
    /// What a tool returned.
    Tool {
        /// The [`ToolCall::id`] this answers.
        call_id: String,
        /// What the tool produced, already bounded.
        content: String,
    },
}

/// A tool the model asked for.
#[derive(Debug, Clone)]
pub struct ToolCall {
    /// The provider's identifier for the call, which its result must name so the
    /// two are paired.
    pub id: String,
    /// The tool's name, which the agent matches against what it offers.
    pub name: String,
    /// The arguments the model emitted.
    pub arguments: Value,
}

/// A tool the agent offers the model.
#[derive(Debug, Clone)]
pub struct Tool {
    /// The name the model calls it by.
    pub name: String,
    /// What it does, which is how the model decides whether to call it.
    pub description: String,
    /// Its arguments, as a JSON schema.
    pub parameters: Value,
}

/// What the model answered for one turn.
#[derive(Debug, Clone)]
pub struct Response {
    /// What it said in prose, which may be empty.
    pub content: String,
    /// The tools it asked for. An empty list means the work is finished and
    /// `content` is the answer.
    pub calls: Vec<ToolCall>,
}

/// Why a model call failed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The model could not be reached, or answered something unusable.
    #[error("the model call failed: {0}")]
    Call(String),
}
