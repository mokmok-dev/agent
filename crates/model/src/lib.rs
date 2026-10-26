//! The model boundary: the port an adapter implements, the retry policy, and
//! the record every request leaves behind.
//!
//! This crate holds no transport. It states what a provider is asked for, what
//! can come back, when a failure may be retried, and which prefix a request
//! occupies in the chain, so the reactor can decide and the ledger can count
//! without either of them knowing how a socket works.

use std::time::Duration;

use agent_core::event::{ErrorClass, FinishReason, Role};
use agent_prompt::{Kind, Prompt};

use agent_core::ids::PrefixHash;

pub mod http;
pub mod openai;

/// How many attempts one request may take, counting the first.
pub const MAX_ATTEMPTS: u32 = 3;

/// The shortest a backoff may be.
pub const BASE_DELAY: Duration = Duration::from_millis(500);

/// What a provider says about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub id: String,
    pub context_window: u64,
    /// Whether the provider reuses the state of a prefix it has already seen.
    pub caches_prefixes: bool,
    /// How long that state is expected to live, when the provider documents it.
    pub cache_ttl: Option<Duration>,
    /// Whether the provider reports how many input tokens it read from cache.
    pub reports_cached_tokens: bool,
}

/// Why a request is being sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    /// A turn the operator asked for.
    Turn,
    /// A minimal request whose only job is to refresh a prefix in the provider's cache.
    Warm,
}

/// One request, as an adapter receives it.
///
/// The parts stay apart rather than arriving as one concatenated string, so an
/// adapter can send a role and a content as separate fields, or add a cache hint
/// to one part, without splitting text the renderer just built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub model: String,
    /// The standing instructions. Becomes the first system message.
    pub directive: String,
    /// The tool block, already ordered and framed by the renderer.
    pub tools: String,
    pub messages: Vec<Message>,
    pub max_output_tokens: u64,
    /// The renderer's estimate of the input size, used only when the provider reports no usage.
    pub input_tokens_estimate: u64,
}

/// One turn of a transcript, as a request carries it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub role: Role,
    pub content: String,
}

impl Request {
    /// Builds a request from a rendered prompt.
    #[must_use]
    pub fn of(
        prompt: &Prompt,
        model: &str,
        max_output_tokens: u64,
    ) -> Self {
        let mut directive = String::new();
        let mut tools = String::new();
        let mut messages = Vec::new();
        for segment in &prompt.segments {
            match segment.kind {
                Kind::Directive => directive.clone_from(&segment.text),
                Kind::Tools => tools.clone_from(&segment.text),
                Kind::Message => {
                    if let Some(role) = segment.role {
                        messages.push(Message {
                            role,
                            content: segment.text.clone(),
                        });
                    }
                },
            }
        }
        Self {
            model: model.to_string(),
            directive,
            tools,
            messages,
            max_output_tokens,
            input_tokens_estimate: prompt.tokens,
        }
    }
}

/// One piece of a response, as it arrives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delta<'a> {
    /// A run of output text. Ephemeral, never appended.
    Text(&'a str),
}

/// A finished response: what it said, and what it cost.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Completion {
    pub usage: Usage,
    /// How the model stopped, when the provider said.
    pub finish: Option<FinishReason>,
}

/// What a finished response cost.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// The input tokens the provider says it read from cache, when it says anything.
    pub cached_tokens: Option<u64>,
    /// Whether the token counts are the provider's numbers or this process's estimate.
    pub estimated: bool,
}

/// Why a request failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderError {
    /// The connection never opened, or died before a response began.
    Transport,
    /// No response within the deadline.
    Timeout,
    /// The provider answered with a status code.
    Status(u16),
    /// The response began and then stopped.
    AfterFirstToken(String),
}

impl ProviderError {
    /// The closed set of classes the log records.
    #[must_use]
    pub fn class(&self) -> ErrorClass {
        match self {
            Self::Transport => ErrorClass::Transport,
            Self::Timeout => ErrorClass::Timeout,
            Self::Status(429) => ErrorClass::RateLimited,
            Self::Status(413) => ErrorClass::ContextLength,
            Self::Status(code) if (500..600).contains(code) => ErrorClass::ServerError,
            Self::Status(code) if (400..500).contains(code) => ErrorClass::InvalidRequest,
            Self::Status(_) | Self::AfterFirstToken(_) => ErrorClass::Unknown,
        }
    }

    /// Whether another attempt is allowed at all, before the attempt count is considered.
    ///
    /// A response that produced a token is never retried. Two attempts could
    /// produce two different answers for one turn, and the operator would have
    /// paid for both.
    #[must_use]
    pub const fn retryable(
        &self,
        produced_token: bool,
    ) -> bool {
        if produced_token {
            return false;
        }
        matches!(
            self,
            Self::Transport | Self::Timeout | Self::Status(429 | 500..=599)
        )
    }
}

/// The wait before the next attempt, or nothing when the attempts are spent.
///
/// `attempt` is the attempt that just failed, counted from one. `draw` is a
/// number the caller takes from its entropy source; the delay is uniform below
/// an exponentially growing ceiling, which is what full jitter means here.
#[must_use]
pub fn backoff(
    attempt: u32,
    draw: u64,
) -> Option<Duration> {
    if attempt == 0 || attempt >= MAX_ATTEMPTS {
        return None;
    }
    let ceiling = BASE_DELAY.as_millis() << (attempt - 1);
    let millis = u64::try_from(ceiling).unwrap_or(u64::MAX);
    Some(Duration::from_millis(draw % millis))
}

/// Where a request sits in the chain, and what it is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestRecord {
    /// The hash of the whole prompt, which is the entry the next request extends.
    pub prefix: PrefixHash,
    pub segments: u32,
    pub tokens: u64,
    pub warm: bool,
}

impl RequestRecord {
    /// Records a rendered prompt.
    ///
    /// Returns nothing when the prompt is empty, which no renderer output is,
    /// and when it has more segments than a record can hold.
    #[must_use]
    pub fn of(
        prompt: &Prompt,
        purpose: Purpose,
    ) -> Option<Self> {
        Some(Self {
            prefix: *prompt.chain.last()?,
            segments: u32::try_from(prompt.segments.len()).ok()?,
            tokens: prompt.tokens,
            warm: purpose == Purpose::Warm,
        })
    }
}

/// What an adapter implements.
pub trait Provider {
    /// What this provider says about itself.
    fn profile(&self) -> Profile;

    /// Sends one request, reporting text as it arrives.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] when no response begins, or when one begins
    /// and stops.
    fn call(
        &mut self,
        request: &Request,
        delta: &mut dyn FnMut(Delta<'_>),
    ) -> Result<Completion, ProviderError>;
}

#[cfg(test)]
mod tests;
