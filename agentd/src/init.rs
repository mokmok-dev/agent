//! First-run setup: create the private config directory and the capability
//! tokens the daemon and its clients need.
//!
//! Only the token file is required to be hand-made; the socket, its parent
//! directory, and the log are created by the daemon on startup (in the runtime
//! and data directories respectively). [`init`]
//! generates a `tokens.json` and one single-secret file per client, all mode
//! `0600`, so `agentd-agent`, `agentd-publish`, and so on can be pointed at
//! them directly. The secrets are returned so they can be shown once, never
//! logged.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use secrecy::zeroize::Zeroizing;
use secrecy::{ExposeSecret as _, SecretString};
use serde::Serialize;
use thiserror::Error;
use uuid::Uuid;

use crate::server::{is_shared_directory, set_mode};

/// The token file name inside the config directory.
pub const TOKEN_FILE: &str = "tokens.json";

/// The provider config file name inside the config directory.
pub const PROVIDERS_FILE: &str = "providers.json";

/// The provider config written on first init.
///
/// It names only a keyless local server (Ollama), so the daemon starts without
/// any credential; a cloud provider is added by editing the file. Writing a
/// provider with an `api_key_env` here would make `serve` fail until that
/// variable is set, so the template deliberately omits one.
const PROVIDERS_TEMPLATE: &str = r#"{
  "providers": {
    "local": {
      "kind": "open_ai_compatible",
      "base_url": "http://127.0.0.1:11434/v1"
    }
  },
  "models": {},
  "default_model": null
}
"#;

/// One generated client: its name, secret, and single-secret file.
#[derive(Debug, Clone)]
pub struct ClientToken {
    /// The client's role, e.g. `agent`.
    pub name: &'static str,
    /// The bearer secret, zeroized on drop and redacted from `Debug`.
    pub secret: SecretString,
    /// The mode-`0600` file holding only this secret.
    pub path: PathBuf,
}

/// The result of [`init`].
#[derive(Debug, Clone)]
pub struct Initialized {
    /// The config directory the files were written to.
    pub config_dir: PathBuf,
    /// The daemon's token file.
    pub tokens_path: PathBuf,
    /// The provider config file.
    pub providers_path: PathBuf,
    /// Whether the provider template was written (`false` when one already
    /// existed and was left untouched).
    pub providers_created: bool,
    /// The generated clients.
    pub clients: Vec<ClientToken>,
}

/// Errors returned by [`init`].
#[derive(Debug, Error)]
pub enum InitError {
    /// Creating a directory or file failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// The token file already exists and `--force` was not given, so an
    /// existing set of capabilities is never silently overwritten.
    #[error("{0} already exists; pass --force to overwrite it")]
    Exists(PathBuf),
    /// The config directory is writable by other users.
    #[error("the config directory {0} is writable by other users")]
    InsecureDirectory(PathBuf),
    /// The token file could not be serialized.
    #[error("failed to encode the token file: {0}")]
    Json(#[from] serde_json::Error),
}

/// The on-disk token file shape written by [`init`].
#[derive(Serialize)]
struct TokenFileWire<'a> {
    tokens: Vec<TokenEntryWire<'a>>,
}

/// One entry of the on-disk token file written by [`init`].
#[derive(Serialize)]
struct TokenEntryWire<'a> {
    secret: &'a str,
    claims: &'a [&'a str],
    source: &'a str,
}

/// The clients [`init`] generates and the claims each is granted.
const CLIENTS: &[(&str, &[&str], &str)] = &[
    ("user", &["read", "publish"], "urn:mokmokd:user"),
    ("agent", &["read", "publish", "infer"], "urn:mokmokd:agent"),
    (
        "admin",
        &["read", "publish", "authority"],
        "urn:mokmokd:admin",
    ),
];

/// Creates the config directory `dir` (mode `0700`) and the token files in it.
///
/// Refuses to overwrite an existing token file unless `force` is set. Any
/// existing client files are rewritten alongside it.
///
/// # Errors
///
/// Returns [`InitError::InsecureDirectory`] if `dir` is writable by other
/// users, [`InitError::Exists`] if the token file exists and `force` is not
/// set, and [`InitError::Io`] if a file cannot be written.
pub fn init(
    dir: &Path,
    force: bool,
) -> Result<Initialized, InitError> {
    if !dir.exists() {
        std::fs::create_dir_all(dir)?;
        set_mode(dir, 0o700)?;
    } else if is_shared_directory(dir) {
        return Err(InitError::InsecureDirectory(dir.to_path_buf()));
    }

    let tokens_path = dir.join(TOKEN_FILE);
    if tokens_path.exists() && !force {
        return Err(InitError::Exists(tokens_path));
    }

    let clients: Vec<ClientToken> = CLIENTS
        .iter()
        .map(|(name, _, _)| ClientToken {
            name,
            secret: generate_secret(),
            path: dir.join(format!("{name}.token")),
        })
        .collect();

    let encoded = Zeroizing::new(serde_json::to_string_pretty(&TokenFileWire {
        tokens: CLIENTS
            .iter()
            .copied()
            .zip(&clients)
            .map(|((_, claims, source), client)| TokenEntryWire {
                secret: client.secret.expose_secret(),
                claims,
                source,
            })
            .collect(),
    })?);
    write_private(&tokens_path, &encoded)?;
    for client in &clients {
        write_private(&client.path, client.secret.expose_secret())?;
    }

    // The provider config is a template the operator edits, so an existing one
    // is never overwritten, even with `--force`.
    let providers_path = dir.join(PROVIDERS_FILE);
    let providers_created = !providers_path.exists();
    if providers_created {
        write_private(&providers_path, PROVIDERS_TEMPLATE)?;
    }

    Ok(Initialized {
        config_dir: dir.to_path_buf(),
        tokens_path,
        providers_path,
        providers_created,
        clients,
    })
}

/// Generates a 256-bit bearer secret as hex.
fn generate_secret() -> SecretString {
    SecretString::from(format!(
        "{}{}",
        Uuid::new_v4().simple(),
        Uuid::new_v4().simple()
    ))
}

/// Writes `contents` to `path`, creating it mode `0600`.
fn write_private(
    path: &Path,
    contents: &str,
) -> Result<(), std::io::Error> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(contents.as_bytes())?;
    // An existing file keeps its mode on open, so set it explicitly too.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{InitError, PROVIDERS_FILE, TOKEN_FILE, init};
    use crate::auth::{Claim, TokenStore};
    use agentd_inference::ProvidersConfig;
    use axum::http::HeaderMap;
    use axum::http::header::AUTHORIZATION;
    use secrecy::ExposeSecret as _;

    /// Builds headers carrying `secret` as a bearer token.
    fn headers(secret: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            format!("Bearer {secret}").parse().expect("header"),
        );
        headers
    }

    #[test]
    fn init_writes_a_private_token_file_and_client_files() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().expect("tempdir should be created");
        let result = init(dir.path(), false).expect("init should succeed");

        assert_eq!(result.tokens_path, dir.path().join(TOKEN_FILE));
        assert_eq!(result.providers_path, dir.path().join(PROVIDERS_FILE));
        assert!(result.providers_created);
        assert_eq!(result.clients.len(), 3);

        let mode = std::fs::metadata(&result.tokens_path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);

        // The file the store loads grants each client its claims.
        let store = TokenStore::load(&result.tokens_path).expect("token file should load");
        let agent = store
            .authorize(&headers(result.clients[1].secret.expose_secret()))
            .expect("agent token should authorize");
        assert!(agent.has(Claim::Infer));
        assert!(!agent.has(Claim::Authority));

        let admin = store
            .authorize(&headers(result.clients[2].secret.expose_secret()))
            .expect("admin token should authorize");
        assert!(admin.has(Claim::Authority));

        // Each single-secret file holds exactly that secret.
        for client in &result.clients {
            let contents = std::fs::read_to_string(&client.path).expect("client file");
            assert_eq!(contents, client.secret.expose_secret());
        }
    }

    #[test]
    fn the_providers_template_is_valid_and_never_overwritten() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let first = init(dir.path(), false).expect("init should succeed");

        // The generated template parses into the real config type.
        let template = std::fs::read_to_string(&first.providers_path).expect("providers file");
        serde_json::from_str::<ProvidersConfig>(&template).expect("template should parse");

        // An operator's edit survives a forced re-init.
        std::fs::write(&first.providers_path, "{\"providers\":{}}").expect("write edit");
        let second = init(dir.path(), true).expect("re-init should succeed");
        assert!(!second.providers_created);
        assert_eq!(
            std::fs::read_to_string(&second.providers_path).expect("providers file"),
            "{\"providers\":{}}"
        );
    }

    #[test]
    fn init_refuses_to_overwrite_without_force() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        init(dir.path(), false).expect("first init should succeed");

        assert!(matches!(init(dir.path(), false), Err(InitError::Exists(_))));
        assert!(init(dir.path(), true).is_ok());
    }

    #[test]
    fn init_refuses_a_shared_directory() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().expect("tempdir should be created");
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o777))
            .expect("chmod");

        assert!(matches!(
            init(dir.path(), false),
            Err(InitError::InsecureDirectory(_))
        ));
    }

    #[test]
    fn secrets_are_distinct() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let result = init(dir.path(), false).expect("init should succeed");

        let mut secrets: Vec<&str> = result
            .clients
            .iter()
            .map(|client| client.secret.expose_secret())
            .collect();
        secrets.sort_unstable();
        secrets.dedup();
        assert_eq!(secrets.len(), result.clients.len());
        assert_eq!(result.clients[0].secret.expose_secret().len(), 64);
    }
}
