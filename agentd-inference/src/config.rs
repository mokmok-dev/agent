//! Provider configuration: named providers, their credentials, and the model
//! routes the daemon resolves a request's `model` against.
//!
//! The config is JSON. A provider names how to reach it; a model route maps an
//! alias an agent asks for onto a concrete provider and model, so a model can be
//! swapped without changing the agent. Credentials come from a private file
//! (mode `0600`, as for the event token) or an environment variable — never
//! inline in the config.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use secrecy::SecretString;
use secrecy::zeroize::Zeroizing;
use serde::Deserialize;
use thiserror::Error;

/// Errors returned while loading providers or their credentials.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// The config file could not be read.
    #[error("failed to read {path}: {source}")]
    Io {
        /// The path that could not be read.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },
    /// The config file was not valid JSON.
    #[error("failed to parse the provider config: {0}")]
    Parse(#[from] serde_json::Error),
    /// A secret file is readable by other users; refusing to load it keeps the
    /// credential private, so the daemon fails closed.
    #[error("the secret file {0} is accessible by other users (mode {1:o})")]
    Permissions(PathBuf, u32),
    /// An `api_key_env` variable is not set.
    #[error("the credential environment variable {0} is not set")]
    MissingEnv(String),
    /// A model route names a provider that is not configured.
    #[error("the model {model:?} routes to the unknown provider {provider:?}")]
    UnknownProvider {
        /// The model alias.
        model: String,
        /// The provider id the alias names.
        provider: String,
    },
}

/// The daemon's provider configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProvidersConfig {
    /// Providers by id.
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderConfig>,
    /// Model aliases the daemon resolves a request's `model` against.
    #[serde(default)]
    pub models: BTreeMap<String, ModelRoute>,
    /// The model used when a request names none.
    #[serde(default)]
    pub default_model: Option<String>,
}

/// One configured provider.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    /// The provider's wire protocol.
    pub kind: ProviderKind,
    /// The API base URL; a kind-specific default is used when absent.
    #[serde(default)]
    pub base_url: Option<String>,
    /// A file holding the bearer credential; must be mode `0600`.
    #[serde(default)]
    pub api_key_file: Option<PathBuf>,
    /// The name of an environment variable holding the credential.
    #[serde(default)]
    pub api_key_env: Option<String>,
}

/// The wire protocol a provider speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    /// The `OpenAI` chat-completions API; also `OpenRouter`, `Ollama`, `vLLM`,
    /// and other compatible servers.
    OpenAiCompatible,
    /// The Anthropic messages API.
    Anthropic,
}

/// A model alias resolved to a concrete provider and model.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelRoute {
    /// The provider id to route to.
    pub provider: String,
    /// The provider's model name.
    pub model: String,
}

impl ProvidersConfig {
    /// Loads a JSON provider config from `path`.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Io`] if the file cannot be read and
    /// [`ConfigError::Parse`] if it is not valid JSON.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let contents = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Ok(serde_json::from_str(&contents)?)
    }
}

impl ProviderConfig {
    /// Resolves the provider's credential, if it has one.
    ///
    /// A local server (for example Ollama) is configured with neither field and
    /// returns `None`. The credential is wrapped in a [`SecretString`], so it is
    /// zeroized on drop and redacted from `Debug`; read it with
    /// `secrecy::ExposeSecret::expose_secret`.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Permissions`] if the secret file is accessible to
    /// other users and [`ConfigError::MissingEnv`] if the environment variable
    /// is not set.
    pub fn credential(&self) -> Result<Option<SecretString>, ConfigError> {
        if let Some(path) = &self.api_key_file {
            return Ok(Some(read_secret(path)?));
        }
        if let Some(name) = &self.api_key_env {
            let value = std::env::var(name).map_err(|_| ConfigError::MissingEnv(name.clone()))?;
            return Ok(Some(SecretString::from(value)));
        }
        Ok(None)
    }
}

/// Reads a secret file, refusing one readable or writable by other users.
///
/// The credential is wrapped so it is zeroized on drop; the parsed file
/// contents are zeroized too.
fn read_secret(path: &Path) -> Result<SecretString, ConfigError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let metadata = std::fs::metadata(path).map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let mode = metadata.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(ConfigError::Permissions(path.to_path_buf(), mode));
        }
    }
    let contents =
        Zeroizing::new(
            std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
                path: path.to_path_buf(),
                source,
            })?,
        );
    Ok(SecretString::from(contents.trim().to_owned()))
}

#[cfg(test)]
mod tests {
    use super::{ConfigError, ProviderKind, ProvidersConfig};
    use secrecy::ExposeSecret as _;
    use serde_json::json;

    #[test]
    fn parses_providers_models_and_default() {
        let raw = json!({
            "providers": {
                "openrouter": {
                    "kind": "open_ai_compatible",
                    "base_url": "https://openrouter.ai/api/v1",
                    "api_key_env": "OPENROUTER_API_KEY"
                },
                "anthropic": { "kind": "anthropic" }
            },
            "models": {
                "fast": { "provider": "openrouter", "model": "openai/gpt-4o-mini" }
            },
            "default_model": "fast"
        })
        .to_string();

        let config: ProvidersConfig = serde_json::from_str(&raw).expect("config should parse");

        assert_eq!(
            config.providers["openrouter"].kind,
            ProviderKind::OpenAiCompatible
        );
        assert_eq!(config.providers["anthropic"].kind, ProviderKind::Anthropic);
        assert_eq!(config.models["fast"].provider, "openrouter");
        assert_eq!(config.default_model.as_deref(), Some("fast"));
    }

    #[test]
    fn an_unknown_field_is_rejected() {
        let raw = json!({
            "providers": {
                "p": { "kind": "anthropic", "api_key_fil": "/x" }
            }
        })
        .to_string();

        assert!(serde_json::from_str::<ProvidersConfig>(&raw).is_err());
    }

    #[test]
    fn a_secret_file_must_not_be_shared() {
        use std::io::Write as _;
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().expect("tempdir should be created");
        let path = dir.path().join("key");
        let mut file = std::fs::File::create(&path).expect("create");
        file.write_all(b"secret").expect("write");
        file.set_permissions(std::fs::Permissions::from_mode(0o644))
            .expect("chmod");

        assert!(matches!(
            super::read_secret(&path),
            Err(ConfigError::Permissions(_, _))
        ));

        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .expect("chmod");
        assert_eq!(
            super::read_secret(&path).expect("read").expose_secret(),
            "secret"
        );
    }
}
