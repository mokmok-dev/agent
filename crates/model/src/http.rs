//! The socket. Everything that decides is somewhere else.
//!
//! This file opens a connection, reads it in chunks, and hands those chunks to
//! the wire parser and the codec. It owns no policy: no retry, no prompt, no
//! record. That is why it can be read in one sitting.

use std::io::Read;
use std::time::Duration;

use agent_prompt::estimate_tokens_of_bytes;
use agent_wire::{Event as Frame, Parser};

use super::openai;
use super::{Completion, Delta, Profile, Provider, ProviderError, Request, Usage};

/// How much of the stream to read at a time.
const READ_BUFFER: usize = 8192;

/// A provider that speaks the `OpenAI` chat-completions shape over HTTP.
pub struct HttpProvider {
    agent: ureq::Agent,
    url: String,
    api_key: Option<String>,
    profile: Profile,
}

impl HttpProvider {
    /// Points an adapter at a base URL, which the adapter appends the endpoint to.
    #[must_use]
    pub fn new(
        profile: Profile,
        base_url: &str,
        api_key: Option<String>,
        timeout: Duration,
    ) -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .build();
        Self {
            agent: ureq::Agent::new_with_config(config),
            url: format!("{}/chat/completions", base_url.trim_end_matches('/')),
            api_key,
            profile,
        }
    }
}

impl Provider for HttpProvider {
    fn profile(&self) -> Profile {
        self.profile.clone()
    }

    fn call(
        &mut self,
        request: &Request,
        delta: &mut dyn FnMut(Delta<'_>),
    ) -> Result<Completion, ProviderError> {
        let mut send = self
            .agent
            .post(&self.url)
            .header("content-type", "application/json")
            .header("accept", "text/event-stream");
        if let Some(key) = &self.api_key {
            send = send.header("authorization", format!("Bearer {key}"));
        }
        let body = openai::encode(request);
        let mut response = send
            .send(body.as_bytes())
            .map_err(|error| refusal(&error))?;
        let mut reader = response.body_mut().as_reader();

        let mut parser = Parser::new();
        let mut buffer = [0_u8; READ_BUFFER];
        let mut produced = false;
        let mut emitted = 0_usize;
        let mut reported = None;
        let mut finish = None;
        let mut calls = openai::Calls::default();

        loop {
            let read = reader
                .read(&mut buffer)
                .map_err(|error| interruption(produced, &error.to_string()))?;
            if let 0 = read {
                break;
            }
            let frames = parser
                .push(&buffer[..read])
                .map_err(|error| interruption(produced, &error.to_string()))?;
            for frame in frames {
                let Frame::Data(data) = frame;
                let chunk = openai::decode(&data)
                    .map_err(|error| interruption(produced, &error.to_string()))?;
                if chunk.done {
                    return Ok(conclusion(
                        request,
                        emitted,
                        reported,
                        finish,
                        calls.finish(),
                    ));
                }
                if let Some(text) = chunk.text {
                    produced = true;
                    emitted = emitted.saturating_add(text.len());
                    delta(Delta::Text(&text));
                }
                if !chunk.calls.is_empty() {
                    calls.absorb(&chunk.calls);
                }
                if let Some(reason) = chunk.finish {
                    finish = Some(openai::finish_of(&reason));
                }
                if let Some(usage) = chunk.usage {
                    reported = Some(Usage {
                        input_tokens: usage.input,
                        output_tokens: usage.output,
                        cached_tokens: usage.cached,
                        estimated: false,
                    });
                }
            }
        }

        if finish.is_none() {
            return Err(interruption(produced, "the stream ended before a finish"));
        }
        Ok(conclusion(
            request,
            emitted,
            reported,
            finish,
            calls.finish(),
        ))
    }
}

/// Turns what arrived into a completion, estimating only when the provider said nothing.
fn conclusion(
    request: &Request,
    emitted: usize,
    reported: Option<Usage>,
    finish: Option<agent_core::event::FinishReason>,
    calls: Vec<openai::Call>,
) -> Completion {
    let usage = reported.unwrap_or(Usage {
        input_tokens: request.input_tokens_estimate,
        output_tokens: estimate_tokens_of_bytes(emitted as u64),
        cached_tokens: None,
        estimated: true,
    });
    Completion {
        usage,
        finish,
        calls,
    }
}

/// Maps a failure before any byte of the body arrived.
const fn refusal(error: &ureq::Error) -> ProviderError {
    match error {
        ureq::Error::StatusCode(code) => ProviderError::Status(*code),
        ureq::Error::Timeout(_) => ProviderError::Timeout,
        _ => ProviderError::Transport,
    }
}

/// Maps a failure after the request was accepted.
///
/// Once a token has been emitted the turn cannot be retried, because a retry
/// could produce a different answer and the operator would pay for both.
fn interruption(
    produced: bool,
    detail: &str,
) -> ProviderError {
    if produced {
        ProviderError::AfterFirstToken(String::from(detail))
    } else {
        ProviderError::Transport
    }
}

#[cfg(test)]
mod tests;
