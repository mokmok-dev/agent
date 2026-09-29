//! The daemon's settings: the OpenAI-compatible endpoints a session may call.
//!
//! The settings file is the operator's declaration of where the agent's model
//! lives, and the session's egress allowlist is **derived** from it: one
//! `host:port` rule per declared endpoint's `base_url`. A separate allowlist
//! could name a host the endpoint does not use, or miss the one it does, and the
//! agent would then reach nothing while the file looked right; deriving the rule
//! from the URL removes that class of mistake. See `docs/session/agent.md`.
//!
//! # Why only `https`
//!
//! The session's egress proxy speaks `CONNECT` only, and the sandbox gives the
//! command no route but that proxy, so a plaintext `http` endpoint cannot be
//! reached at all. A URL that names one is refused when the file is read, rather
//! than failing later where the failure looks like a network outage. See
//! `docs/sandbox/network.md`.
//!
//! # Why the directories are arguments
//!
//! [`config_path`] takes the two directory variables as arguments instead of
//! reading them, so the resolution is a pure function with tests. The binary that
//! loads the settings reads `XDG_CONFIG_HOME` and `HOME` and passes them in.
//! Setting an environment variable from a test needs `unsafe` in edition 2024,
//! which this workspace denies everywhere.
//!
//! # The file
//!
//! ```toml
//! # The model a session uses, and the id sent to the API.
//! default = "grok-4.7"
//!
//! [model."grok-4.7"]
//! base_url = "https://api.x.ai/v1"
//! env_key = "XAI_API_KEY"
//! ```
//!
//! A file that declares endpoints must name which one a session uses, because the
//! agent is told exactly one model at launch. Validation happens once, in
//! [`Settings::parse`], so [`Settings::egress_rules`] cannot fail. An absent file
//! is an empty [`Settings`], which grants no egress and names no model — the same
//! fail-closed default an empty allowlist has.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use sandbox::policy::HostPort;
use serde::Deserialize;

use crate::image::AgentImage;

/// The file name the settings live in.
const CONFIG_FILE: &str = "config.toml";

/// The directory under the configuration home that holds the file.
const CONFIG_DIR: &str = "agent";

/// The port an `https` URL without one reaches.
const HTTPS_PORT: u16 = 443;

/// The path of the settings file, or `None` when neither directory is known.
///
/// `$XDG_CONFIG_HOME/agent/config.toml`, falling back to
/// `$HOME/.config/agent/config.toml`. A relative `XDG_CONFIG_HOME` is invalid per
/// the XDG base directory specification, so it is ignored and the home directory
/// is used instead.
#[must_use]
pub fn config_path(
    xdg: Option<&Path>,
    home: Option<&Path>,
) -> Option<PathBuf> {
    let home = home.map(|home| home.join(".config"));
    let base = xdg
        .filter(|directory| directory.is_absolute())
        .or(home.as_deref())?;
    Some(base.join(CONFIG_DIR).join(CONFIG_FILE))
}

/// The daemon's settings.
#[derive(Debug, Clone, Default)]
pub struct Settings {
    /// The model a session uses, which a file that declares endpoints must name.
    default: Option<String>,
    /// The declared endpoints, keyed by the model id sent to the API.
    models: BTreeMap<String, Endpoint>,
    /// The egress rules the endpoints derive, in model-id order and deduplicated.
    rules: Vec<HostPort>,
}

/// One OpenAI-compatible endpoint, as the file declares it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Endpoint {
    /// The endpoint's base URL. Only `https` is accepted.
    pub base_url: String,
    /// The environment variable that holds the API key. The daemon only names
    /// it; it never reads the value, because the key is the agent's own and the
    /// tunnel is opaque. See `docs/sandbox/network.md`.
    pub env_key: String,
}

/// The file as it is written, before validation.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    /// The model a session uses.
    #[serde(default)]
    default: Option<String>,
    /// The declared endpoints, keyed by model id.
    #[serde(default)]
    model: BTreeMap<String, Endpoint>,
}

impl Settings {
    /// Parse `text` as the settings file.
    ///
    /// Validates every endpoint and derives the egress rules here, so everything
    /// after this call is infallible.
    ///
    /// # Errors
    ///
    /// Returns [`Error`] for text that is not valid TOML, a `default` that names no
    /// declared endpoint, a file that declares endpoints but no `default`, a
    /// `base_url` that names no destination a session could reach, or an `env_key`
    /// that is not a variable name.
    pub fn parse(text: &str) -> Result<Self, Error> {
        let raw: Raw = toml::from_str(text)?;
        // A session is told exactly one model, so a file that declares endpoints
        // must say which one it is.
        if let Some(model) = &raw.default {
            if !raw.model.contains_key(model) {
                return Err(Error::UndeclaredModel {
                    model: model.clone(),
                });
            }
        } else if !raw.model.is_empty() {
            return Err(Error::MissingDefault);
        }
        let mut rules: Vec<HostPort> = Vec::new();
        for (model, endpoint) in &raw.model {
            let destination = destination(model, &endpoint.base_url)?;
            validate_env_key(model, &endpoint.env_key)?;
            // The set is matched on `host:port`, so one endpoint's rule stands
            // for every endpoint that names the same destination.
            if !rules.contains(&destination) {
                rules.push(destination);
            }
        }
        Ok(Self {
            default: raw.default,
            models: raw.model,
            rules,
        })
    }

    /// Read and parse the settings file at `path`.
    ///
    /// A file that does not exist is an empty [`Settings`]: the operator has
    /// declared no endpoint, so no session reaches one.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Read`] when the path cannot be read, and whatever
    /// [`Settings::parse`] returns when its contents are invalid.
    pub fn load_from(path: &Path) -> Result<Self, Error> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            },
            Err(source) => {
                return Err(Error::Read {
                    path: path.to_path_buf(),
                    source,
                });
            },
        };
        Self::parse(&text)
    }

    /// The endpoint declared for `model`, if the file declares one.
    #[must_use]
    pub fn endpoint(
        &self,
        model: &str,
    ) -> Option<&Endpoint> {
        self.models.get(model)
    }

    /// The egress rules the declared endpoints derive, for a session's proxy and
    /// its `ProxyGrant`.
    #[must_use]
    pub fn egress_rules(&self) -> &[HostPort] {
        &self.rules
    }

    /// The model a session uses, or `None` when the file declares no endpoint.
    #[must_use]
    pub fn default_model(&self) -> Option<&str> {
        self.default.as_deref()
    }

    /// The image for a session of the project's agent, running `program`.
    ///
    /// The image carries what the agent needs to reach its endpoint: the model, the
    /// base URL, and the key's variable name in its argv; the key's **value** in its
    /// environment; and the egress rules the endpoints derive.
    ///
    /// The value comes from `key` rather than from the environment this runs in, so
    /// the caller decides where a key lives and a test can answer from a map. A
    /// report names the variable and never the value.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NoModel`] when the file declares no endpoint to use, and
    /// [`Error::MissingKey`] when `key` has no value for the variable the endpoint
    /// names.
    pub fn image(
        &self,
        program: impl Into<PathBuf>,
        key: impl FnOnce(&str) -> Option<String>,
    ) -> Result<AgentImage, Error> {
        let Some(model) = self.default.as_deref() else {
            return Err(Error::NoModel);
        };
        // Validation rejects a `default` that names no declared endpoint, so this
        // cannot be missing in a parsed file.
        let Some(endpoint) = self.models.get(model) else {
            return Err(Error::NoModel);
        };
        let value = key(&endpoint.env_key).ok_or_else(|| Error::MissingKey {
            name: endpoint.env_key.clone(),
        })?;

        let mut image = AgentImage::agent(program);
        for rule in &self.rules {
            image = image.allowing_destination(rule.clone());
        }
        Ok(image
            .with_arg("--model")
            .with_arg(model)
            .with_arg("--base-url")
            .with_arg(endpoint.base_url.as_str())
            .with_arg("--env-key")
            .with_arg(endpoint.env_key.as_str())
            .with_env(endpoint.env_key.as_str(), value))
    }
}

/// Why the settings could not be loaded.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The settings file could not be read.
    #[error("the settings file `{path}` could not be read: {source}")]
    Read {
        /// The path that was read.
        path: PathBuf,
        /// The failure the filesystem reported.
        source: std::io::Error,
    },
    /// The settings file is not valid TOML.
    #[error("the settings file is not valid TOML: {0}")]
    Toml(#[from] toml::de::Error),
    /// An endpoint's base URL has no scheme.
    #[error("`{model}` has no scheme in its base URL `{base_url}`")]
    MissingScheme {
        /// The model whose endpoint is malformed.
        model: String,
        /// The URL as written.
        base_url: String,
    },
    /// An endpoint's base URL names a scheme the proxy cannot carry.
    #[error("`{model}` names the scheme `{scheme}`; the egress proxy speaks only `CONNECT`")]
    NotHttps {
        /// The model whose endpoint is malformed.
        model: String,
        /// The scheme as written.
        scheme: String,
    },
    /// An endpoint's base URL carries credentials.
    #[error("`{model}` names credentials in its base URL; the key belongs in its environment")]
    Credentials {
        /// The model whose endpoint is malformed.
        model: String,
    },
    /// An endpoint's base URL names no host a session could reach.
    #[error("`{model}` names no usable host in `{base_url}`")]
    Host {
        /// The model whose endpoint is malformed.
        model: String,
        /// The URL as written.
        base_url: String,
    },
    /// An endpoint's base URL names no usable port.
    #[error("`{model}` names no usable port in its base URL")]
    BadPort {
        /// The model whose endpoint is malformed.
        model: String,
    },
    /// An endpoint's `env_key` is not a variable name.
    #[error("`{model}` names an environment variable that is empty or contains `=`")]
    EnvKey {
        /// The model whose endpoint is malformed.
        model: String,
    },
    /// The file declares endpoints but no model to use.
    #[error("the settings declare endpoints but no `default` model")]
    MissingDefault,
    /// The `default` names a model the file does not declare.
    #[error("`{model}` is not a declared model")]
    UndeclaredModel {
        /// The model id the file named.
        model: String,
    },
    /// There is no endpoint to build a session's image from.
    #[error("the settings declare no model endpoint")]
    NoModel,
    /// The key's value is not available to the caller that builds the image.
    #[error("`{name}` is not set, so a session would have no key for its endpoint")]
    MissingKey {
        /// The variable the endpoint names.
        name: String,
    },
}

/// The destination an endpoint's `base_url` names.
///
/// The rule is the URL's own authority: only `https` is accepted, an explicit
/// port is kept, and everything after the authority is discarded, because the
/// allowlist matches on `host:port` alone.
///
/// A host the proxy could never match is refused rather than stored, rather than
/// leaving a rule that looks right and permits nothing: the match is byte-wise and
/// case-insensitive, so a host that is not ASCII could not match the punycode a
/// client sends, and a host carrying a `:` is an IPv6 literal whose brackets the
/// proxy unwraps before it matches.
fn destination(
    model: &str,
    base_url: &str,
) -> Result<HostPort, Error> {
    let Some((scheme, rest)) = base_url.split_once("://") else {
        return Err(Error::MissingScheme {
            model: model.to_owned(),
            base_url: base_url.to_owned(),
        });
    };
    if !scheme.eq_ignore_ascii_case("https") {
        return Err(Error::NotHttps {
            model: model.to_owned(),
            scheme: scheme.to_owned(),
        });
    }
    // The authority ends at the first `/`, `?`, or `#`; the rest is the path,
    // the query, or the fragment.
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..end];
    if authority.contains('@') {
        return Err(Error::Credentials {
            model: model.to_owned(),
        });
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (
            host,
            port.parse::<u16>().map_err(|_| Error::BadPort {
                model: model.to_owned(),
            })?,
        ),
        None => (authority, HTTPS_PORT),
    };
    let host_is_usable = !host.is_empty()
        && host.is_ascii()
        && !host.contains(':')
        && !host.chars().any(|ch| ch.is_whitespace() || ch == '\0');
    if !host_is_usable {
        return Err(Error::Host {
            model: model.to_owned(),
            base_url: base_url.to_owned(),
        });
    }
    // The port must not be zero, which `HostPort` rejects inside a policy and
    // which no destination uses.
    if port == 0 {
        return Err(Error::BadPort {
            model: model.to_owned(),
        });
    }
    // A host is matched case-insensitively but stored by exact equality, so it is
    // folded here and two spellings of one host become one rule.
    Ok(HostPort::new(host.to_ascii_lowercase(), port))
}

/// Check that `env_key` names an environment variable, by the same rule an
/// [`EnvVar`](sandbox::policy::EnvVar) follows.
fn validate_env_key(
    model: &str,
    env_key: &str,
) -> Result<(), Error> {
    if env_key.is_empty() || env_key.contains('=') {
        return Err(Error::EnvKey {
            model: model.to_owned(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    // Tests for reading the file, refusing an endpoint a session could not
    // reach, and deriving the allowlist from the URLs. Every branch of the URL
    // split has its own case: the paths and ports are where a wrong bound would
    // otherwise pass unnoticed.

    use super::*;
    use sandbox::policy::EnvVar;

    /// A settings file with one endpoint.
    fn text(base_url: &str) -> String {
        format!(
            r#"
default = "grok-4.7"

[model."grok-4.7"]
base_url = "{base_url}"
env_key = "XAI_API_KEY"
"#
        )
    }

    /// The single rule a one-endpoint file derives.
    fn rule(base_url: &str) -> HostPort {
        let settings = Settings::parse(&text(base_url)).expect("parses");
        let rules = settings.egress_rules().to_vec();
        assert_eq!(rules.len(), 1, "one endpoint derives one rule");
        rules[0].clone()
    }

    /// A path under the temp directory, unique to this process and `tag`.
    fn temp(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("daemon-settings-{tag}-{}", std::process::id()))
    }

    #[test]
    fn a_base_url_without_a_path_derives_https_port_443() {
        assert_eq!(rule("https://api.x.ai"), HostPort::new("api.x.ai", 443));
    }

    #[test]
    fn a_base_url_with_a_path_derives_the_authority() {
        assert_eq!(rule("https://api.x.ai/v1"), HostPort::new("api.x.ai", 443));
    }

    #[test]
    fn a_trailing_slash_derives_the_authority() {
        assert_eq!(rule("https://api.x.ai/"), HostPort::new("api.x.ai", 443));
    }

    #[test]
    fn a_query_or_fragment_derives_the_authority() {
        assert_eq!(
            rule("https://api.x.ai?key=1"),
            HostPort::new("api.x.ai", 443)
        );
        assert_eq!(rule("https://api.x.ai#top"), HostPort::new("api.x.ai", 443));
    }

    #[test]
    fn an_explicit_port_is_kept() {
        assert_eq!(
            rule("https://api.example.com:8443/v1"),
            HostPort::new("api.example.com", 8443)
        );
    }

    #[test]
    fn an_uppercase_scheme_and_host_are_folded() {
        assert_eq!(
            rule("HTTPS://API.Example.COM/v1"),
            HostPort::new("api.example.com", 443)
        );
    }

    #[test]
    fn every_declared_endpoint_derives_a_rule() {
        let settings = Settings::parse(
            r#"
default = "grok-4.7"

[model."grok-4.7"]
base_url = "https://api.x.ai/v1"
env_key = "XAI_API_KEY"

[model."local"]
base_url = "https://models.internal:8443/v1"
env_key = "LOCAL_API_KEY"
"#,
        )
        .expect("parses");
        assert_eq!(
            settings.egress_rules(),
            [
                HostPort::new("api.x.ai", 443),
                HostPort::new("models.internal", 8443)
            ]
        );
    }

    #[test]
    fn one_destination_declared_twice_is_one_rule() {
        let settings = Settings::parse(
            r#"
default = "a"

[model."a"]
base_url = "https://api.x.ai/v1"
env_key = "A_KEY"

[model."b"]
base_url = "https://API.x.ai/v2"
env_key = "B_KEY"
"#,
        )
        .expect("parses");
        assert_eq!(settings.egress_rules(), [HostPort::new("api.x.ai", 443)]);
    }

    #[test]
    fn the_same_host_on_two_ports_is_two_rules() {
        let settings = Settings::parse(
            r#"
default = "a"

[model."a"]
base_url = "https://api.x.ai/v1"
env_key = "A_KEY"

[model."b"]
base_url = "https://api.x.ai:8443/v1"
env_key = "B_KEY"
"#,
        )
        .expect("parses");
        assert_eq!(
            settings.egress_rules(),
            [
                HostPort::new("api.x.ai", 443),
                HostPort::new("api.x.ai", 8443)
            ]
        );
    }

    #[test]
    fn an_endpoint_keeps_its_base_url_and_env_key() {
        let settings = Settings::parse(&text("https://api.x.ai/v1")).expect("parses");
        let endpoint = settings.endpoint("grok-4.7").expect("declared");
        assert_eq!(endpoint.base_url, "https://api.x.ai/v1");
        assert_eq!(endpoint.env_key, "XAI_API_KEY");
    }

    #[test]
    fn an_undeclared_model_has_no_endpoint() {
        let settings = Settings::parse(&text("https://api.x.ai/v1")).expect("parses");
        assert!(settings.endpoint("gpt-9").is_none());
    }

    #[test]
    fn the_default_names_the_model_a_session_uses() {
        let settings = Settings::parse(&text("https://api.x.ai/v1")).expect("parses");
        assert_eq!(settings.default_model(), Some("grok-4.7"));
    }

    #[test]
    fn a_file_that_declares_endpoints_must_name_one() {
        // A session is told exactly one model, so the file has to say which.
        let text = r#"
[model."grok-4.7"]
base_url = "https://api.x.ai/v1"
env_key = "XAI_API_KEY"
"#;
        let error = Settings::parse(text).expect_err("refused");
        assert!(
            matches!(error, Error::MissingDefault),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn a_default_that_names_no_endpoint_is_refused() {
        let text = r#"
default = "gpt-9"

[model."grok-4.7"]
base_url = "https://api.x.ai/v1"
env_key = "XAI_API_KEY"
"#;
        let error = Settings::parse(text).expect_err("refused");
        assert!(
            matches!(&error, Error::UndeclaredModel { model } if model == "gpt-9"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn a_file_with_no_endpoints_names_no_model() {
        let settings = Settings::parse("").expect("parses");
        assert_eq!(settings.default_model(), None);
        assert!(
            matches!(
                settings.image("/usr/bin/agent-agent", |_| None),
                Err(Error::NoModel)
            ),
            "a session with no endpoint has no image to run"
        );
    }

    #[test]
    fn an_image_carries_the_endpoint_and_the_key() {
        let settings = Settings::parse(&text("https://api.x.ai/v1")).expect("parses");
        let image = settings
            .image("/usr/bin/agent-agent", |name| {
                (name == "XAI_API_KEY").then(|| "the-key".to_owned())
            })
            .expect("builds");
        assert_eq!(image.program, PathBuf::from("/usr/bin/agent-agent"));
        assert_eq!(
            image.args,
            [
                "--model",
                "grok-4.7",
                "--base-url",
                "https://api.x.ai/v1",
                "--env-key",
                "XAI_API_KEY"
            ],
            "the agent is told which model to call and how to authenticate"
        );
        assert_eq!(image.env, [EnvVar::new("XAI_API_KEY", "the-key")]);
        assert_eq!(
            image.egress_rules(),
            [HostPort::new("api.x.ai", 443)],
            "the endpoint's destination is the session's one rule"
        );
    }

    #[test]
    fn an_image_without_a_key_value_is_refused_by_name() {
        let settings = Settings::parse(&text("https://api.x.ai/v1")).expect("parses");
        let error = settings
            .image("/usr/bin/agent-agent", |_| None)
            .expect_err("refused");
        assert!(
            matches!(&error, Error::MissingKey { name } if name == "XAI_API_KEY"),
            "unexpected error: {error}"
        );
        assert!(
            error.to_string().contains("XAI_API_KEY"),
            "the variable is named: {error}"
        );
    }

    #[test]
    fn an_empty_file_declares_nothing() {
        let settings = Settings::parse("").expect("parses");
        assert!(settings.egress_rules().is_empty());
        assert!(settings.endpoint("grok-4.7").is_none());
    }

    #[test]
    fn a_plaintext_endpoint_is_refused() {
        let error = Settings::parse(&text("http://api.x.ai/v1")).expect_err("refused");
        assert!(
            matches!(&error, Error::NotHttps { model, scheme } if model == "grok-4.7" && scheme == "http"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn an_endpoint_without_a_scheme_is_refused() {
        let error = Settings::parse(&text("api.x.ai/v1")).expect_err("refused");
        assert!(
            matches!(&error, Error::MissingScheme { base_url, .. } if base_url == "api.x.ai/v1"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn an_endpoint_with_credentials_is_refused() {
        let error = Settings::parse(&text("https://key@api.x.ai/v1")).expect_err("refused");
        assert!(
            matches!(error, Error::Credentials { .. }),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn an_endpoint_without_a_host_is_refused() {
        for base_url in ["https:///v1", "https://", "https://:8443/v1"] {
            let error = Settings::parse(&text(base_url)).expect_err("refused");
            assert!(
                matches!(error, Error::Host { .. } | Error::BadPort { .. }),
                "`{base_url}` gave: {error}"
            );
        }
    }

    #[test]
    fn a_bracketed_ipv6_host_is_refused() {
        // The proxy unwraps the brackets before it matches, so a bracketed rule
        // could never permit the connection it names.
        let error = Settings::parse(&text("https://[::1]:8443/v1")).expect_err("refused");
        assert!(
            matches!(error, Error::Host { .. }),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn a_host_that_is_not_ascii_is_refused() {
        // A client sends a `CONNECT` target as punycode, which a rule holding the
        // Unicode form would never match.
        let error = Settings::parse(&text("https://апи.пример/v1")).expect_err("refused");
        assert!(
            matches!(error, Error::Host { .. }),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn a_host_with_whitespace_is_refused() {
        // The policy would reject it too, but a session must fail at load rather
        // than when it builds itself.
        let error = Settings::parse(&text("https://api example.com/v1")).expect_err("refused");
        assert!(
            matches!(error, Error::Host { .. }),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn an_unusable_port_is_refused() {
        for base_url in [
            "https://api.x.ai:0/v1",
            "https://api.x.ai:/v1",
            "https://api.x.ai:https/v1",
            "https://api.x.ai:99999/v1",
        ] {
            let error = Settings::parse(&text(base_url)).expect_err("refused");
            assert!(
                matches!(error, Error::BadPort { .. }),
                "`{base_url}` gave: {error}"
            );
        }
    }

    #[test]
    fn an_unusable_env_key_is_refused() {
        for env_key in ["", "XAI=API_KEY"] {
            let text = format!(
                r#"
default = "grok-4.7"

[model."grok-4.7"]
base_url = "https://api.x.ai/v1"
env_key = "{env_key}"
"#
            );
            let error = Settings::parse(&text).expect_err("refused");
            assert!(
                matches!(error, Error::EnvKey { .. }),
                "`{env_key}` gave: {error}"
            );
        }
    }

    #[test]
    fn an_endpoint_without_a_base_url_is_refused() {
        let text = r#"
[model."grok-4.7"]
env_key = "XAI_API_KEY"
"#;
        let error = Settings::parse(text).expect_err("refused");
        assert!(matches!(error, Error::Toml(_)), "unexpected error: {error}");
    }

    #[test]
    fn an_unknown_key_is_refused() {
        let text = r#"
[model."grok-4.7"]
base_url = "https://api.x.ai/v1"
env_key = "XAI_API_KEY"
temperature = 0.7
"#;
        let error = Settings::parse(text).expect_err("refused");
        assert!(matches!(error, Error::Toml(_)), "unexpected error: {error}");
    }

    #[test]
    fn text_that_is_not_toml_is_refused() {
        let error = Settings::parse("this is not = toml [").expect_err("refused");
        assert!(matches!(error, Error::Toml(_)), "unexpected error: {error}");
    }

    #[test]
    fn a_missing_file_is_empty_settings() {
        let path = temp("missing").join("config.toml");
        let settings = Settings::load_from(&path).expect("a missing file is not an error");
        assert!(settings.egress_rules().is_empty());
    }

    #[test]
    fn an_existing_file_is_parsed() {
        let path = temp("existing").join("config.toml");
        let directory = path.parent().expect("has a parent").to_path_buf();
        std::fs::create_dir_all(&directory).expect("creates the directory");
        std::fs::write(&path, text("https://api.x.ai/v1")).expect("writes the file");
        let settings = Settings::load_from(&path).expect("loads");
        assert_eq!(settings.egress_rules(), [HostPort::new("api.x.ai", 443)]);
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_path_that_is_not_a_file_is_refused_with_its_path() {
        let directory = temp("directory");
        std::fs::create_dir_all(&directory).expect("creates the directory");
        let error = Settings::load_from(&directory).expect_err("refused");
        assert!(
            matches!(&error, Error::Read { path, .. } if path == &directory),
            "unexpected error: {error}"
        );
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn an_invalid_file_is_refused() {
        let path = temp("invalid").join("config.toml");
        let directory = path.parent().expect("has a parent").to_path_buf();
        std::fs::create_dir_all(&directory).expect("creates the directory");
        std::fs::write(&path, "base_url = [").expect("writes the file");
        let error = Settings::load_from(&path).expect_err("refused");
        assert!(matches!(error, Error::Toml(_)), "unexpected error: {error}");
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn the_config_path_prefers_the_xdg_directory() {
        let path = config_path(Some(Path::new("/xdg")), Some(Path::new("/home/me")));
        assert_eq!(path, Some(PathBuf::from("/xdg/agent/config.toml")));
    }

    #[test]
    fn the_config_path_falls_back_to_the_home_directory() {
        let path = config_path(None, Some(Path::new("/home/me")));
        assert_eq!(
            path,
            Some(PathBuf::from("/home/me/.config/agent/config.toml"))
        );
    }

    #[test]
    fn a_relative_xdg_directory_is_ignored() {
        let path = config_path(Some(Path::new("xdg")), Some(Path::new("/home/me")));
        assert_eq!(
            path,
            Some(PathBuf::from("/home/me/.config/agent/config.toml"))
        );
    }

    #[test]
    fn without_either_directory_there_is_no_config_path() {
        assert_eq!(config_path(None, None), None);
        assert_eq!(config_path(Some(Path::new("xdg")), None), None);
    }
}
