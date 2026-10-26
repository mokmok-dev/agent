//! The `OpenAI` chat-completions shape, as bytes.
//!
//! Everything here is a pure function of its input: a request becomes a JSON
//! body, a data frame becomes text, a finish reason, and usage. The adapter that
//! owns the socket has nothing left to decide but the connection.

use serde::{Deserialize, Serialize};

use super::Request;
use agent_core::event::{FinishReason, Role};

/// The provider's own end-of-stream sentinel, which is not a JSON frame.
pub const DONE: &str = "[DONE]";

/// A request body, in the shape the provider expects.
#[derive(Serialize)]
struct Body<'a> {
    model: &'a str,
    messages: Vec<WireMessage<'a>>,
    stream: bool,
    stream_options: StreamOptions,
    max_tokens: u64,
}

#[derive(Serialize)]
struct WireMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Serialize)]
struct StreamOptions {
    include_usage: bool,
}

/// Encodes one request.
///
/// The tool block goes in as a second system message rather than in a `tools`
/// field, because the renderer hashed it as one segment and the chain must keep
/// describing the bytes the provider actually reads. An adapter that sends
/// schemas as structured tools would change those bytes, and would have to say
/// so in the renderer rather than here.
#[must_use]
pub fn encode(request: &Request) -> String {
    let mut messages = Vec::with_capacity(request.messages.len().saturating_add(2));
    messages.push(WireMessage {
        role: Role::System.as_str(),
        content: &request.directive,
    });
    if !request.tools.is_empty() {
        messages.push(WireMessage {
            role: Role::System.as_str(),
            content: &request.tools,
        });
    }
    for message in &request.messages {
        messages.push(WireMessage {
            role: message.role.as_str(),
            content: &message.content,
        });
    }
    let body = Body {
        model: &request.model,
        messages,
        stream: true,
        stream_options: StreamOptions {
            include_usage: true,
        },
        max_tokens: request.max_output_tokens,
    };
    serde_json::to_string(&body).unwrap_or_default()
}

/// The finish reason a provider's word means.
///
/// The four the log knows are mapped exactly. Anything else is recorded as
/// unknown rather than folded into one of them, because a provider that stops
/// for a reason this build has never seen is a fact worth keeping.
#[must_use]
pub fn finish_of(reason: &str) -> FinishReason {
    match reason {
        "stop" => FinishReason::Stop,
        "length" => FinishReason::Length,
        "tool_calls" | "function_call" => FinishReason::ToolCalls,
        "content_filter" => FinishReason::ContentFilter,
        _ => FinishReason::Unknown,
    }
}

/// One data frame, read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Chunk {
    /// The text this frame added, when it added any.
    pub text: Option<String>,
    /// The provider's own finish reason, untranslated.
    pub finish: Option<String>,
    /// Token counts, when this frame is the one that carries them.
    pub usage: Option<Usage>,
    /// Whether this frame is the end-of-stream sentinel.
    pub done: bool,
    /// Pieces of the tools the model is asking for, as the provider splits them.
    pub calls: Vec<CallFrag>,
}

/// One piece of one tool call.
///
/// A provider may send the id and name in the first frame for an index and only
/// the arguments after it, so nothing here is required to be present.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CallFrag {
    pub index: u32,
    pub id: Option<String>,
    pub name: Option<String>,
    pub arguments: Option<String>,
}

/// A tool call the model asked for, whole.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Call {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// Collects the pieces of a call as they stream in.
#[derive(Debug, Default)]
pub struct Calls {
    seen: Vec<Call>,
}

impl Calls {
    /// Takes one frame's pieces.
    pub fn absorb(
        &mut self,
        fragments: &[CallFrag],
    ) {
        for fragment in fragments {
            let at = fragment.index as usize;
            while self.seen.len() <= at {
                self.seen.push(Call {
                    id: String::new(),
                    name: String::new(),
                    arguments: String::new(),
                });
            }
            let call = &mut self.seen[at];
            if let Some(id) = &fragment.id {
                call.id.clone_from(id);
            }
            if let Some(name) = &fragment.name {
                call.name.clone_from(name);
            }
            if let Some(arguments) = &fragment.arguments {
                call.arguments.push_str(arguments);
            }
        }
    }

    /// Everything that arrived.
    #[must_use]
    pub fn finish(self) -> Vec<Call> {
        self.seen
    }
}

/// Counts as the provider wrote them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cached: Option<u64>,
}

/// Why a frame could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodecError {
    /// The frame was not the JSON this provider documents.
    Json(String),
}

impl std::fmt::Display for CodecError {
    fn fmt(
        &self,
        formatter: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        match self {
            Self::Json(detail) => {
                write!(formatter, "a data frame is not the expected JSON: {detail}")
            },
        }
    }
}

impl std::error::Error for CodecError {}

#[derive(Deserialize)]
struct Frame {
    #[serde(default)]
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<WireUsage>,
}

#[derive(Deserialize)]
struct Choice {
    #[serde(default)]
    delta: Option<Delta>,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct Delta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<WireCall>,
}

#[derive(Deserialize)]
struct WireCall {
    #[serde(default)]
    index: u32,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<WireFunction>,
}

#[derive(Deserialize)]
struct WireFunction {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Deserialize)]
struct WireUsage {
    prompt_tokens: u64,
    completion_tokens: u64,
    #[serde(default)]
    prompt_tokens_details: Option<CachedDetails>,
}

#[derive(Deserialize)]
struct CachedDetails {
    #[serde(default)]
    cached_tokens: Option<u64>,
}

/// Reads one data frame.
///
/// # Errors
///
/// Returns [`CodecError::Json`] when the frame is not the documented shape. A
/// frame the provider adds later, with fields this build does not know, is not
/// an error: unknown fields are ignored.
pub fn decode(data: &str) -> Result<Chunk, CodecError> {
    if data == DONE {
        return Ok(Chunk {
            done: true,
            ..Chunk::default()
        });
    }
    let frame: Frame =
        serde_json::from_str(data).map_err(|error| CodecError::Json(error.to_string()))?;
    let mut chunk = Chunk::default();
    for choice in &frame.choices {
        if let Some(content) = choice
            .delta
            .as_ref()
            .and_then(|delta| delta.content.as_ref())
        {
            chunk.text = Some(content.clone());
        }
        if choice.finish_reason.is_some() {
            chunk.finish.clone_from(&choice.finish_reason);
        }
        if let Some(delta) = &choice.delta {
            for piece in &delta.tool_calls {
                chunk.calls.push(CallFrag {
                    index: piece.index,
                    id: piece.id.clone(),
                    name: piece
                        .function
                        .as_ref()
                        .and_then(|function| function.name.clone()),
                    arguments: piece
                        .function
                        .as_ref()
                        .and_then(|function| function.arguments.clone()),
                });
            }
        }
    }
    if let Some(usage) = &frame.usage {
        chunk.usage = Some(Usage {
            input: usage.prompt_tokens,
            output: usage.completion_tokens,
            cached: usage
                .prompt_tokens_details
                .as_ref()
                .and_then(|details| details.cached_tokens),
        });
    }
    Ok(chunk)
}

#[cfg(test)]
mod tests;
