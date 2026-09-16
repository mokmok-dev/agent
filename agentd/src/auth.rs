//! Client authentication and authorization for the event API.
//!
//! The daemon exposes a Unix domain socket, so the first boundary is the
//! filesystem: the runtime directory is private to the daemon user (see
//! [`crate::server::run`]). On top of that, every connection presents a bearer
//! token whose [`Claim`]s decide what it may do — subscribe to the log
//! ([`Claim::Read`]), append events ([`Claim::Publish`]), publish the
//! reserved daemon-authority types such as `sandbox.permission.*`
//! ([`Claim::Authority`]), or request model inference ([`Claim::Infer`]).
//!
//! Tokens are a *capability*: a client that must not publish authoritatively —
//! above all an agent running inside a sandbox — should be denied read access to
//! the token file (see `docs/sandbox.md` and `docs/architecture.md`). Until the
//! sandbox is wired in, this module still enforces the protocol split and keeps
//! the token file private to the daemon user.

use std::collections::BTreeSet;
use std::path::Path;

use axum::http::HeaderMap;
use axum::http::header::AUTHORIZATION;
use serde::{Deserialize, Serialize};
use thiserror::Error as ThisError;

/// A capability a token grants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Claim {
    /// Subscribe to the event stream (replay and live).
    Read,
    /// Append non-reserved events to the log.
    Publish,
    /// Publish the reserved daemon-authority types (`error.*`, `sandbox.*`,
    /// `session.*`); implies the intent to publish.
    Authority,
    /// Request model inference through the daemon's `/inference` endpoint. A
    /// token holding only this claim may not read or publish events.
    Infer,
}

/// An authenticated connection: what it may do and the `source` its events are
/// stamped with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    claims: BTreeSet<Claim>,
    source: String,
}

impl Principal {
    /// Creates a principal with `claims` whose events carry `source`.
    #[must_use]
    pub fn new(
        source: impl Into<String>,
        claims: impl IntoIterator<Item = Claim>,
    ) -> Self {
        Self {
            claims: claims.into_iter().collect(),
            source: source.into(),
        }
    }

    /// Whether the principal holds `claim`.
    #[must_use]
    pub fn has(
        &self,
        claim: Claim,
    ) -> bool {
        self.claims.contains(&claim)
    }

    /// The `CloudEvents` `source` the daemon stamps on this principal's events.
    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }
}

/// A token together with what it grants.
#[derive(Clone, PartialEq, Eq)]
pub struct Token {
    /// The bearer secret.
    pub secret: String,
    /// The capability the secret confers.
    pub principal: Principal,
}

impl std::fmt::Debug for Token {
    /// Redacts the secret, so a `Token` can never leak it through logging.
    fn fmt(
        &self,
        f: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        f.debug_struct("Token")
            .field("secret", &"<redacted>")
            .field("principal", &self.principal)
            .finish()
    }
}

/// Errors returned while loading or applying a [`TokenStore`].
#[derive(Debug, ThisError)]
pub enum AuthError {
    /// The token file could not be read.
    #[error("failed to read the token file: {0}")]
    Io(#[from] std::io::Error),
    /// The token file was not valid JSON, or a claim name was unknown.
    #[error("failed to parse the token file: {0}")]
    Parse(#[from] serde_json::Error),
    /// The token file is readable or writable by other users; refusing to load
    /// it would keep the capability secret, so the daemon fails closed.
    #[error("the token file {0} is accessible by other users (mode {1:o})")]
    Permissions(std::path::PathBuf, u32),
    /// The connection presented no bearer token.
    #[error("the request is missing a bearer token")]
    MissingToken,
    /// The connection presented a token that is not configured.
    #[error("the bearer token is not recognized")]
    UnknownToken,
    /// The connection holds no usable claim.
    #[error("the bearer token grants no access to the event API")]
    NoAccess,
}

/// The configured tokens, indexed for constant-time lookup.
#[derive(Debug, Clone)]
pub struct TokenStore {
    tokens: Vec<Token>,
}

impl TokenStore {
    /// Creates a store from an explicit list of tokens.
    #[must_use]
    pub const fn new(tokens: Vec<Token>) -> Self {
        Self { tokens }
    }

    /// Loads the token file at `path`.
    ///
    /// The file is a JSON object `{ "tokens": [ { "secret", "claims",
    /// "source" } ] }`. On Unix the file must not be accessible by group or
    /// other users, or loading fails closed.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError::Io`], [`AuthError::Parse`], or
    /// [`AuthError::Permissions`].
    pub fn load(path: &Path) -> Result<Self, AuthError> {
        let metadata = std::fs::metadata(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = metadata.permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                return Err(AuthError::Permissions(path.to_path_buf(), mode));
            }
        }
        let _ = metadata;

        let contents = std::fs::read_to_string(path)?;
        let file: TokenFile = serde_json::from_str(&contents)?;
        Ok(Self {
            tokens: file
                .tokens
                .into_iter()
                .map(|entry| Token {
                    secret: entry.secret,
                    principal: Principal::new(entry.source, entry.claims),
                })
                .collect(),
        })
    }

    /// Authorizes a connection from its headers.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError::MissingToken`] when no `Authorization: Bearer` is
    /// present, [`AuthError::UnknownToken`] when the secret is not configured,
    /// and [`AuthError::NoAccess`] when the matched token grants none of read,
    /// publish, or inference.
    pub fn authorize(
        &self,
        headers: &HeaderMap,
    ) -> Result<Principal, AuthError> {
        let token = bearer(headers).ok_or(AuthError::MissingToken)?;
        // Compare against every configured token without short-circuiting, so
        // the position of a match is not observable through timing.
        let mut matched: Option<&Principal> = None;
        for candidate in &self.tokens {
            if constant_time_eq(candidate.secret.as_bytes(), token.as_bytes()) {
                matched = Some(&candidate.principal);
            }
        }
        let principal = matched.ok_or(AuthError::UnknownToken)?;
        if !principal.has(Claim::Read)
            && !principal.has(Claim::Publish)
            && !principal.has(Claim::Infer)
        {
            return Err(AuthError::NoAccess);
        }
        Ok(principal.clone())
    }
}

/// Extracts the bearer secret from `Authorization: Bearer <token>`.
fn bearer(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(AUTHORIZATION)?.to_str().ok()?;
    let (scheme, secret) = value.split_once(' ')?;
    (scheme.eq_ignore_ascii_case("bearer") && !secret.is_empty()).then_some(secret)
}

/// A comparison whose duration depends only on the inputs' lengths.
fn constant_time_eq(
    a: &[u8],
    b: &[u8],
) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The on-disk token file shape.
#[derive(Debug, Deserialize)]
struct TokenFile {
    tokens: Vec<TokenFileEntry>,
}

/// One entry of the on-disk token file.
#[derive(Debug, Deserialize)]
struct TokenFileEntry {
    secret: String,
    claims: Vec<Claim>,
    source: String,
}

#[cfg(test)]
mod tests {
    use super::{AuthError, Claim, Principal, Token, TokenStore};
    use axum::http::HeaderMap;
    use axum::http::header::AUTHORIZATION;

    /// Builds headers carrying `secret` as a bearer token.
    fn headers(secret: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            format!("Bearer {secret}").parse().expect("header"),
        );
        headers
    }

    fn store() -> TokenStore {
        TokenStore::new(vec![
            Token {
                secret: String::from("read-secret"),
                principal: Principal::new("urn:test:reader", [Claim::Read]),
            },
            Token {
                secret: String::from("authority-secret"),
                principal: Principal::new(
                    "urn:test:approver",
                    [Claim::Read, Claim::Publish, Claim::Authority],
                ),
            },
        ])
    }

    #[test]
    fn authorizes_a_known_token() {
        let principal = store()
            .authorize(&headers("authority-secret"))
            .expect("known token");

        assert!(principal.has(Claim::Authority));
        assert_eq!(principal.source(), "urn:test:approver");
    }

    #[test]
    fn bearer_scheme_is_case_insensitive() {
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, "bearer read-secret".parse().expect("header"));

        assert!(store().authorize(&headers).is_ok());
    }

    #[test]
    fn token_debug_redacts_the_secret() {
        let token = Token {
            secret: String::from("super-secret"),
            principal: Principal::new("urn:test", [Claim::Read]),
        };

        assert!(!format!("{token:?}").contains("super-secret"));
    }

    #[test]
    fn rejects_missing_and_unknown_tokens() {
        assert!(matches!(
            store().authorize(&HeaderMap::new()),
            Err(AuthError::MissingToken)
        ));
        assert!(matches!(
            store().authorize(&headers("wrong")),
            Err(AuthError::UnknownToken)
        ));
        assert!(matches!(
            store().authorize(&headers("")),
            Err(AuthError::MissingToken)
        ));
    }

    #[test]
    fn read_only_tokens_hold_no_publish_claim() {
        let principal = store()
            .authorize(&headers("read-secret"))
            .expect("known token");

        assert!(principal.has(Claim::Read));
        assert!(!principal.has(Claim::Publish));
        assert!(!principal.has(Claim::Authority));
    }

    #[test]
    fn a_token_without_read_or_publish_is_refused() {
        let store = TokenStore::new(vec![Token {
            secret: String::from("nothing"),
            principal: Principal::new("urn:test:none", []),
        }]);

        assert!(matches!(
            store.authorize(&headers("nothing")),
            Err(AuthError::NoAccess)
        ));
    }

    #[test]
    fn loads_a_token_file_and_enforces_privacy() {
        use std::io::Write as _;
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("tokens.json");
        let mut file = std::fs::File::create(&path).expect("create");
        file.write_all(
            br#"{"tokens":[{"secret":"s","claims":["read","publish"],"source":"urn:test"}]}"#,
        )
        .expect("write");
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .expect("chmod");

        let store = TokenStore::load(&path).expect("private file should load");
        assert!(store.authorize(&headers("s")).is_ok());

        file.set_permissions(std::fs::Permissions::from_mode(0o644))
            .expect("chmod");
        assert!(matches!(
            TokenStore::load(&path),
            Err(AuthError::Permissions(_, _))
        ));
    }
}
