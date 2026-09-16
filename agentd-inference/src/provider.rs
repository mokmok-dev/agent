//! The [`Provider`] abstraction and the deterministic [`FakeProvider`].
//!
//! The daemon selects a provider and streams from it; a real provider is an
//! adapter that maps its wire format onto the [`Delta`] stream. The fake
//! provider replays a scripted list of deltas, so the agent loop can be tested
//! without a network or a model.

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::Mutex;

use async_trait::async_trait;
use futures_util::Stream;
use thiserror::Error;

use crate::wire::{Delta, InferenceRequest};

/// The reason a provider could not start or continue a stream.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ProviderError {
    /// The provider failed to produce a response.
    #[error("the provider failed: {0}")]
    Failed(String),
}

/// A stream of response deltas ending at the first terminal delta.
pub type InferenceStream = Pin<Box<dyn Stream<Item = Result<Delta, ProviderError>> + Send>>;

/// A model provider the daemon streams completions from.
///
/// The trait is provider-neutral: an adapter owns its HTTP client and
/// credentials and exposes only the [`Delta`] stream.
#[async_trait]
pub trait Provider: Send + Sync + 'static {
    /// Starts a completion for `request`.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] when the request cannot be started. A failure
    /// after the stream has started arrives as a [`Delta::Error`] item instead.
    async fn stream(
        &self,
        request: InferenceRequest,
    ) -> Result<InferenceStream, ProviderError>;
}

/// A provider that replays scripted deltas, for tests and offline development.
///
/// Each `stream` call pops the next script. When the scripts are exhausted the
/// fake replies with one text delta and [`Delta::Done`], so a daemon started
/// without a script still completes a turn.
#[derive(Debug, Default)]
pub struct FakeProvider {
    scripts: Mutex<VecDeque<Vec<Delta>>>,
}

impl FakeProvider {
    /// Creates a fake that replays `scripts` in order, one per `stream` call.
    #[must_use]
    pub fn new(scripts: Vec<Vec<Delta>>) -> Self {
        Self {
            scripts: Mutex::new(scripts.into()),
        }
    }

    /// The fallback response when a script is exhausted.
    fn fallback() -> Vec<Delta> {
        vec![
            Delta::Text {
                text: String::from("fake provider: no scripted response"),
            },
            Delta::Done {
                finish_reason: Some(String::from("stop")),
            },
        ]
    }
}

#[async_trait]
impl Provider for FakeProvider {
    async fn stream(
        &self,
        _request: InferenceRequest,
    ) -> Result<InferenceStream, ProviderError> {
        let script = self
            .scripts
            .lock()
            .map_err(|_| {
                ProviderError::Failed(String::from("the fake provider lock was poisoned"))
            })?
            .pop_front()
            .unwrap_or_else(Self::fallback);
        Ok(Box::pin(futures_util::stream::iter(
            script.into_iter().map(Ok),
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::{FakeProvider, Provider};
    use crate::wire::{Delta, InferenceRequest};
    use futures_util::StreamExt;

    fn request() -> InferenceRequest {
        InferenceRequest {
            messages: Vec::new(),
            tools: Vec::new(),
        }
    }

    fn done() -> Delta {
        Delta::Done {
            finish_reason: None,
        }
    }

    #[tokio::test]
    async fn fake_provider_replays_each_script_in_order() {
        let provider = FakeProvider::new(vec![
            vec![
                Delta::Text {
                    text: String::from("first"),
                },
                done(),
            ],
            vec![
                Delta::Text {
                    text: String::from("second"),
                },
                done(),
            ],
        ]);

        let first: Vec<_> = provider
            .stream(request())
            .await
            .expect("first stream should start")
            .collect()
            .await;
        let second: Vec<_> = provider
            .stream(request())
            .await
            .expect("second stream should start")
            .collect()
            .await;

        assert_eq!(
            first.into_iter().filter_map(Result::ok).collect::<Vec<_>>(),
            vec![
                Delta::Text {
                    text: String::from("first")
                },
                done()
            ]
        );
        assert_eq!(
            second
                .into_iter()
                .filter_map(Result::ok)
                .collect::<Vec<_>>(),
            vec![
                Delta::Text {
                    text: String::from("second")
                },
                done()
            ]
        );
    }

    #[tokio::test]
    async fn fake_provider_falls_back_when_scripts_run_out() {
        let provider = FakeProvider::default();

        let deltas: Vec<_> = provider
            .stream(request())
            .await
            .expect("stream should start")
            .collect()
            .await;

        assert_eq!(deltas.len(), 2);
        assert!(matches!(&deltas[0], Ok(Delta::Text { text }) if text.contains("no scripted")));
        assert!(matches!(&deltas[1], Ok(Delta::Done { .. })));
    }
}
