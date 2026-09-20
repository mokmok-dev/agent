//! The default filesystem locations shared by the daemon and its clients.
//!
//! They follow the XDG Base Directory specification, so state is not scattered
//! directly in the home directory:
//!
//! | Kind    | Variable            | Default                | Holds                     |
//! | ------- | ------------------- | ---------------------- | ------------------------- |
//! | config  | `XDG_CONFIG_HOME`   | `~/.config/agentd`     | `tokens.json`, `providers.json` |
//! | data    | `XDG_DATA_HOME`     | `~/.local/share/agentd`| `events.jsonl`            |
//! | runtime | `XDG_RUNTIME_DIR`   | `$TMPDIR/agentd`       | `agentd.sock`             |
//!
//! The secrets (tokens) live in config and the durable event log in data, so a
//! backup or a wipe of one does not touch the other. The socket is runtime
//! state and belongs in a private, per-user directory, never in the home.

use std::path::PathBuf;

/// The directory holding the daemon's config, including the token file.
#[must_use]
pub fn config_dir() -> PathBuf {
    base_dir("XDG_CONFIG_HOME", ".config").join("agentd")
}

/// The directory holding the durable event log.
#[must_use]
pub fn data_dir() -> PathBuf {
    base_dir("XDG_DATA_HOME", ".local/share").join("agentd")
}

/// The directory holding the runtime socket.
///
/// `XDG_RUNTIME_DIR` is private to the user (mode `0700`) and is the correct
/// home for a socket; where it is absent (macOS does not set it), the per-user
/// `$TMPDIR` is used instead.
#[must_use]
pub fn runtime_dir() -> PathBuf {
    if let Some(dir) = env_path("XDG_RUNTIME_DIR") {
        return dir.join("agentd");
    }
    if let Some(tmp) = env_path("TMPDIR") {
        return tmp.join("agentd");
    }
    std::env::temp_dir().join("agentd")
}

/// The default event socket path.
#[must_use]
pub fn default_socket() -> PathBuf {
    runtime_dir().join("agentd.sock")
}

/// The default durable event log path.
#[must_use]
pub fn default_log() -> PathBuf {
    data_dir().join("events.jsonl")
}

/// The default token file path.
#[must_use]
pub fn default_tokens() -> PathBuf {
    config_dir().join("tokens.json")
}

/// The default provider config path, used when it exists and no explicit path
/// was given.
#[must_use]
pub fn default_providers() -> PathBuf {
    config_dir().join("providers.json")
}

/// The default token file a `publish`-only client presents, written by
/// [`crate::config_dir`]'s `agentd init` beside the daemon's `tokens.json`.
#[must_use]
pub fn default_user_token() -> PathBuf {
    config_dir().join("user.token")
}

/// An XDG base directory, defaulting to `$HOME/<home_suffix>`.
fn base_dir(
    variable: &str,
    home_suffix: &str,
) -> PathBuf {
    if let Some(dir) = env_path(variable) {
        return dir;
    }
    if let Some(home) = env_path("HOME") {
        return home.join(home_suffix);
    }
    std::env::temp_dir()
}

/// Reads an environment variable as an absolute path, ignoring an empty or
/// relative value as the specification requires.
fn env_path(variable: &str) -> Option<PathBuf> {
    let value = std::env::var_os(variable)?;
    if value.is_empty() {
        return None;
    }
    let path = PathBuf::from(value);
    path.is_absolute().then_some(path)
}

#[cfg(test)]
mod tests {
    use super::{
        config_dir, data_dir, default_log, default_socket, default_tokens, default_user_token,
        runtime_dir,
    };
    use std::ffi::OsStr;

    #[test]
    fn defaults_are_absolute_and_named() {
        assert!(config_dir().is_absolute());
        assert!(data_dir().is_absolute());
        assert!(runtime_dir().is_absolute());

        assert_eq!(
            default_socket().file_name().and_then(OsStr::to_str),
            Some("agentd.sock")
        );
        assert_eq!(
            default_log().file_name().and_then(OsStr::to_str),
            Some("events.jsonl")
        );
        assert_eq!(
            default_tokens().file_name().and_then(OsStr::to_str),
            Some("tokens.json")
        );
        assert_eq!(
            default_user_token().file_name().and_then(OsStr::to_str),
            Some("user.token")
        );
    }

    #[test]
    fn config_and_data_are_distinct() {
        assert_ne!(config_dir(), data_dir());
    }
}
