//! The provider registry: it resolves a request's `model` onto a concrete
//! provider and model and delegates the stream.
//!
//! The daemon holds one registry; a sandboxed agent asks for a model by name and
//! never learns a provider id or a credential. This keeps model swaps and,
//! later, fallback routing on the daemon side, where the events and the
//! credentials are.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;

use crate::config::{ConfigError, ModelRoute, ProvidersConfig};
use crate::provider::{InferenceStream, Provider, ProviderError};
use crate::providers;
use crate::wire::InferenceRequest;

/// A set of providers plus the model routes between them.
pub struct ProviderRegistry {
    providers: BTreeMap<String, Arc<dyn Provider>>,
    models: BTreeMap<String, ModelRoute>,
    default_model: Option<String>,
}

impl std::fmt::Debug for ProviderRegistry {
    /// Lists the configured providers without rendering their credentials.
    fn fmt(
        &self,
        f: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        f.debug_struct("ProviderRegistry")
            .field("providers", &format_args!("{:?}", self.providers.keys()))
            .field("models", &format_args!("{:?}", self.models.keys()))
            .field("default_model", &self.default_model)
            .finish()
    }
}

impl ProviderRegistry {
    /// Builds a registry, constructing one adapter per configured provider.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] if a provider's credential cannot be resolved.
    pub fn new(config: ProvidersConfig) -> Result<Self, ConfigError> {
        let mut providers = BTreeMap::new();
        for (id, provider_config) in config.providers {
            providers.insert(id, providers::build(&provider_config)?);
        }
        if let Some((model, route)) = config
            .models
            .iter()
            .find(|(_, route)| !providers.contains_key(&route.provider))
        {
            return Err(ConfigError::UnknownProvider {
                model: model.clone(),
                provider: route.provider.clone(),
            });
        }
        Ok(Self {
            providers,
            default_model: config.default_model,
            models: config.models,
        })
    }

    /// Resolves `requested` to a provider and concrete model.
    ///
    /// A configured alias wins; otherwise a `provider/model` pair is split;
    /// otherwise a sole provider receives the name unchanged.
    fn resolve(
        &self,
        requested: Option<&str>,
    ) -> Result<(&Arc<dyn Provider>, String), ProviderError> {
        let name = requested.or(self.default_model.as_deref()).ok_or_else(|| {
            ProviderError::Failed(String::from(
                "no model was requested and no default_model is configured",
            ))
        })?;
        if let Some(route) = self.models.get(name) {
            let provider = self.providers.get(&route.provider).ok_or_else(|| {
                ProviderError::Failed(format!(
                    "the model {name:?} routes to the unknown provider {:?}",
                    route.provider
                ))
            })?;
            return Ok((provider, route.model.clone()));
        }
        if let Some((provider_id, model)) = name.split_once('/')
            && let Some(provider) = self.providers.get(provider_id)
        {
            return Ok((provider, model.to_string()));
        }
        if self.providers.len() == 1
            && let Some((_, provider)) = self.providers.iter().next()
        {
            return Ok((provider, name.to_string()));
        }
        Err(ProviderError::Failed(format!(
            "unknown model {name:?}; configure a model route or use provider/model"
        )))
    }
}

#[async_trait]
impl Provider for ProviderRegistry {
    async fn stream(
        &self,
        mut request: InferenceRequest,
    ) -> Result<InferenceStream, ProviderError> {
        let (provider, model) = self.resolve(request.model.as_deref())?;
        request.model = Some(model);
        provider.stream(request).await
    }
}

#[cfg(test)]
mod tests {
    use super::ProviderRegistry;
    use crate::config::{ProviderConfig, ProviderKind, ProvidersConfig};
    use crate::provider::FakeProvider;

    fn registry() -> ProviderRegistry {
        let config: ProvidersConfig = serde_json::from_value(serde_json::json!({
            "providers": {
                "primary": { "kind": "open_ai_compatible" },
                "secondary": { "kind": "open_ai_compatible" }
            },
            "models": {
                "fast": { "provider": "secondary", "model": "tiny" }
            },
            "default_model": "fast"
        }))
        .expect("config should parse");
        // Providers are real adapters here; only resolution is exercised.
        let mut registry = ProviderRegistry::new(config).expect("registry should build");
        registry.providers.insert(
            String::from("primary"),
            std::sync::Arc::new(FakeProvider::default()),
        );
        registry.providers.insert(
            String::from("secondary"),
            std::sync::Arc::new(FakeProvider::default()),
        );
        registry
    }

    #[test]
    fn an_alias_resolves_to_its_route() {
        let registry = registry();
        let (_provider, model) = registry.resolve(Some("fast")).expect("resolve");

        assert_eq!(model, "tiny");
    }

    #[test]
    fn a_provider_prefixed_name_splits() {
        let registry = registry();
        let (provider, model) = registry.resolve(Some("primary/gpt-4o")).expect("resolve");

        assert!(std::sync::Arc::ptr_eq(
            provider,
            registry.providers.get("primary").expect("primary")
        ));
        assert_eq!(model, "gpt-4o");
    }

    #[test]
    fn the_default_model_is_used_when_none_is_requested() {
        let registry = registry();
        let (_provider, model) = registry.resolve(None).expect("resolve");

        assert_eq!(model, "tiny");
    }

    #[test]
    fn a_sole_provider_receives_the_bare_name() {
        let config: ProvidersConfig = serde_json::from_value(serde_json::json!({
            "providers": { "only": { "kind": "open_ai_compatible" } }
        }))
        .expect("config should parse");
        let mut registry = ProviderRegistry::new(config).expect("registry should build");
        registry.providers.insert(
            String::from("only"),
            std::sync::Arc::new(FakeProvider::default()),
        );

        let (_provider, model) = registry.resolve(Some("llama3")).expect("resolve");
        assert_eq!(model, "llama3");
    }

    #[test]
    fn an_unknown_model_is_an_error() {
        let registry = registry();
        assert!(registry.resolve(Some("mystery")).is_err());
    }

    #[test]
    fn a_route_to_an_unknown_provider_is_rejected() {
        let config: ProvidersConfig = serde_json::from_value(serde_json::json!({
            "providers": { "p": { "kind": "open_ai_compatible" } },
            "models": { "m": { "provider": "nope", "model": "x" } }
        }))
        .expect("config should parse");

        assert!(ProviderRegistry::new(config).is_err());
    }

    #[test]
    fn provider_configs_default_their_base_url() {
        let config: ProviderConfig =
            serde_json::from_value(serde_json::json!({ "kind": "anthropic" }))
                .expect("config should parse");
        assert_eq!(config.kind, ProviderKind::Anthropic);
        assert_eq!(config.base_url, None);
    }
}
