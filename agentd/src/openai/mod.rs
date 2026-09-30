//! The OpenAI-compatible `chat/completions` client: the first [`Model`].
//!
//! The agent's model is a [`Model`] seam, so this is one implementation of it and
//! the capability that drives a model never learns what a `role`, a `tool_call`,
//! or a `Bearer` header is. `wire` owns the mapping and `http` owns the transport;
//! this module ties them together and is the only thing the binary names.
//!
//! # What the transport is up against
//!
//! Inside a session the agent has no route but the egress proxy, which
//! authenticates with `Proxy-Authorization: Bearer <token>` and answers `CONNECT`
//! only. That, plus a trust store no file in the sandbox can supply, is why the
//! transport is written out rather than delegated to an HTTP client.
//!
//! # Bounds
//!
//! One call reads at most a mebibyte of reply under one deadline
//! ([`DEFAULT_DEADLINE`] unless a caller says otherwise), and the reply is read
//! **under** the cap rather than after it. A model's context and this process's
//! memory are both resources, and both are bounded the same way; see
//! `docs/session/agent.md` and decision-trail row 14.
//!
//! # The key
//!
//! The key is a constructor argument and never reaches a log: the [`Debug`] impl
//! of [`Client`] redacts it, as does [`http::Proxy`] with the proxy's token. The
//! binary reads the value from the environment, which is where the design keeps it
//! (the daemon copies it into the session's environment and holds no copy of its
//! own).

mod http;
mod wire;

use std::sync::Arc;
use std::time::{Duration, Instant};

use rustls::{ClientConfig, RootCertStore};

use crate::model::{self, Message, Model, Response, Tool};

pub use http::Proxy;

/// How long one model call may take, end to end.
pub const DEFAULT_DEADLINE: Duration = Duration::from_secs(60);

/// What a client needs to reach one endpoint.
#[derive(Debug)]
pub struct Config {
    /// The endpoint's base URL, `https://host[:port][/path]`. The completions path
    /// is appended to it.
    pub base_url: String,
    /// The model id every request asks for.
    pub model: String,
    /// The API key, sent as `Authorization: Bearer <key>`.
    pub api_key: String,
    /// The proxy to reach the endpoint through, or `None` to connect directly.
    proxy: Option<Proxy>,
    /// The roots the endpoint's certificate must chain to, or `None` for the ones
    /// compiled into the binary.
    roots: Option<RootCertStore>,
    /// How long one call may take, end to end.
    deadline: Duration,
}

impl Config {
    /// A config for `model` at `base_url`, holding `api_key`, with no proxy, the
    /// compiled roots, and [`DEFAULT_DEADLINE`].
    #[must_use]
    pub fn new(
        base_url: impl Into<String>,
        model: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            model: model.into(),
            api_key: api_key.into(),
            proxy: None,
            roots: None,
            deadline: DEFAULT_DEADLINE,
        }
    }

    /// Reach the endpoint through `proxy`.
    #[must_use]
    pub fn with_proxy(
        mut self,
        proxy: Proxy,
    ) -> Self {
        self.proxy = Some(proxy);
        self
    }

    /// Trust `roots` instead of the compiled set.
    ///
    /// The compiled set is what a session uses, because no CA file is readable
    /// there. A caller that does have its own trust store — a private CA, or a test
    /// against a local server — names it here.
    #[must_use]
    pub fn with_roots(
        mut self,
        roots: RootCertStore,
    ) -> Self {
        self.roots = Some(roots);
        self
    }

    /// Give one call at most `deadline`.
    #[must_use]
    pub const fn with_deadline(
        mut self,
        deadline: Duration,
    ) -> Self {
        self.deadline = deadline;
        self
    }
}

/// A client for one OpenAI-compatible endpoint.
pub struct Client {
    /// The TLS configuration, built once from the roots.
    tls: Arc<ClientConfig>,
    /// Where the requests go.
    endpoint: http::Target,
    /// The model id every request asks for.
    model: String,
    /// The `Authorization` value, which carries the key.
    authorization: String,
    /// The proxy to reach the endpoint through.
    proxy: Option<Proxy>,
    /// How long one call may take.
    deadline: Duration,
}

impl Client {
    /// A client for `config`.
    ///
    /// The endpoint is checked here rather than on the first call, so a session
    /// whose settings name an unusable endpoint fails when it starts.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Endpoint`] for a base URL this client cannot use.
    pub fn new(config: Config) -> Result<Self, Error> {
        let roots = config.roots.unwrap_or_else(http::compiled_roots);
        let tls = Arc::new(
            ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth(),
        );
        Ok(Self {
            tls,
            endpoint: http::Target::parse(&config.base_url)?,
            model: config.model,
            authorization: format!("Bearer {}", config.api_key),
            proxy: config.proxy,
            deadline: config.deadline,
        })
    }

    /// Ask the endpoint for the next turn of the conversation.
    ///
    /// # Errors
    ///
    /// Returns [`Error`] naming the step that failed: the request could not be
    /// built, the endpoint could not be reached or trusted, the deadline passed, a
    /// head or body exceeded its bound, or the provider answered with an error.
    pub fn call(
        &self,
        messages: &[Message],
        tools: &[Tool],
    ) -> Result<Response, Error> {
        let body = wire::Request::new(&self.model, messages, tools)?.to_bytes()?;
        let reply = http::exchange(
            &self.tls,
            &self.endpoint,
            self.proxy.as_ref(),
            &self.authorization,
            &body,
            Instant::now() + self.deadline,
        )?;
        if !(200..300).contains(&reply.status) {
            return Err(Error::Provider {
                status: reply.status,
                reason: http::reason(&reply.body),
            });
        }
        wire::parse(&reply.body)?.into_response()
    }
}

impl Model for Client {
    fn complete(
        &self,
        messages: &[Message],
        tools: &[Tool],
    ) -> Result<Response, model::Error> {
        self.call(messages, tools)
            .map_err(|error| model::Error::Call(error.to_string()))
    }
}

impl std::fmt::Debug for Client {
    fn fmt(
        &self,
        f: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        // Never the key: `Model` requires `Debug`, and a Debug string reaches logs.
        f.debug_struct("Client")
            .field("endpoint", &self.endpoint)
            .field("model", &self.model)
            .field("authorization", &"<redacted>")
            .field("proxy", &self.proxy)
            .field("deadline", &self.deadline)
            .finish_non_exhaustive()
    }
}

/// Why a model call could not be made or understood.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The endpoint's base URL is not one this client can use.
    #[error("the endpoint `{0}` is not usable")]
    Endpoint(String),
    /// The proxy URL is not one this client can use.
    #[error("the proxy `{0}` is not usable")]
    ProxyUrl(String),
    /// The proxy refused the tunnel, so the endpoint was never reached.
    #[error("the proxy answered {status}: {reason}")]
    Proxy {
        /// The status the proxy answered with.
        status: u16,
        /// What the proxy said, in its own words when it gave any.
        reason: String,
    },
    /// The provider answered the request with an error.
    #[error("the provider answered {status}: {reason}")]
    Provider {
        /// The status the provider answered with.
        status: u16,
        /// What the provider said, in its own words when it gave any.
        reason: String,
    },
    /// The request could not be built.
    #[error("the request could not be built: {0}")]
    Request(String),
    /// The reply could not be understood.
    #[error("{0}")]
    Reply(String),
    /// The endpoint could not be reached, or TLS could not be established.
    #[error("the endpoint could not be reached: {0}")]
    Transport(#[from] std::io::Error),
    /// The deadline passed before the exchange finished.
    #[error("the model call did not finish within its deadline")]
    Deadline,
    /// The reply, or its head, was larger than this client reads.
    #[error("the {what} was larger than {limit} bytes")]
    TooLarge {
        /// Which part of the reply: `head` or `reply`.
        what: &'static str,
        /// The bound it crossed.
        limit: usize,
    },
}

#[cfg(test)]
mod tests {
    // Tests for building a client: the endpoint is checked up front, and the key
    // never reaches a Debug string. The calls themselves are driven over real
    // sockets in `agentd/tests/openai.rs`.

    use super::*;

    /// A config for a client that is never called.
    fn config() -> Config {
        Config::new("https://api.x.ai/v1", "grok-4.7", "xai-secret")
    }

    #[test]
    fn a_client_builds_for_a_usable_endpoint() {
        let client = Client::new(config()).expect("builds");
        assert_eq!(client.model, "grok-4.7");
        assert_eq!(client.authorization, "Bearer xai-secret");
        assert_eq!(client.endpoint.path, "/v1/chat/completions");
    }

    #[test]
    fn an_unusable_endpoint_fails_when_the_client_is_built() {
        let error = Client::new(Config::new("api.x.ai/v1", "m", "k")).expect_err("refused");
        assert!(matches!(error, Error::Endpoint(_)), "gave: {error}");
    }

    #[test]
    fn a_clients_debug_never_shows_the_key() {
        let client = Client::new(config()).expect("builds");
        let debug = format!("{client:?}");
        assert!(!debug.contains("xai-secret"), "debug was: {debug}");
        assert!(debug.contains("redacted"), "debug was: {debug}");
    }

    #[test]
    fn a_config_defaults_to_the_compiled_roots_and_the_default_deadline() {
        let config = config();
        assert!(config.roots.is_none());
        assert!(config.proxy.is_none());
        assert_eq!(config.deadline, DEFAULT_DEADLINE);
    }
}
