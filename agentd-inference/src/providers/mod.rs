//! The provider adapters, built from configuration.

pub mod anthropic;
pub mod openai;
pub mod sse;

use std::sync::Arc;

use crate::config::{ConfigError, ProviderConfig, ProviderKind};
use crate::provider::Provider;

/// Builds the adapter for `config`, resolving its credential.
///
/// # Errors
///
/// Returns [`ConfigError`] if the credential cannot be resolved.
pub fn build(config: &ProviderConfig) -> Result<Arc<dyn Provider>, ConfigError> {
    let credential = config.credential()?;
    let provider: Arc<dyn Provider> = match config.kind {
        ProviderKind::OpenAiCompatible => Arc::new(openai::OpenAiProvider::new(
            config.base_url.clone(),
            credential,
        )),
        ProviderKind::Anthropic => Arc::new(anthropic::AnthropicProvider::new(
            config.base_url.clone(),
            credential,
        )),
    };
    Ok(provider)
}
