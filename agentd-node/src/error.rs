//! The error type shared by the agent's projection and run loop.

use thiserror::Error;

/// Errors returned while running the agent or projecting its conversation.
#[derive(Debug, Error)]
pub enum AgentError {
    /// A SQLite operation failed.
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    /// A value stored in the projection could not be encoded or decoded.
    #[error("the projection could not encode its state: {0}")]
    Json(#[from] serde_json::Error),
    /// A stored message has a role the reducer does not recognize.
    #[error("the conversation is malformed: {0}")]
    History(String),
    /// Connecting to or speaking with the daemon's event API failed.
    #[error(transparent)]
    Client(#[from] crate::ClientError),
    /// The inference request failed at the transport level.
    #[error(transparent)]
    Inference(#[from] agentd_inference::ClientError),
    /// The provider reported a failure in the response stream.
    #[error("the provider failed: {0}")]
    Provider(String),
    /// The provider did not finish a response within the timeout, so the turn
    /// was abandoned rather than blocking the agent forever.
    #[error("the inference request did not finish within {0:?}")]
    InferenceTimeout(std::time::Duration),
    /// The model kept calling tools without stopping, so the turn was abandoned.
    #[error("the model called tools more than {0} times without finishing")]
    TooManyToolRounds(u32),
}
