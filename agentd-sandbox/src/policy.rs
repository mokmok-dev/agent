//! The sandbox policy model.
//!
//! A [`Policy`] is deny-by-default in every domain: the zero value grants no
//! path access, runs no command, and imposes safe resource limits. The
//! filesystem domain is a single list of path entries evaluated with the
//! precedence `deny > write > read`; the network has no configurability because
//! egress and ingress are denied outright. The policy serializes into JSON so
//! it can arrive as event data; every field defaults to an inert value when
//! absent.

use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error as ThisError;

/// The deny-by-default configuration a sandbox runs under.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Policy {
    /// Filesystem policy: path entries and protected metadata names.
    pub fs: FsPolicy,
    /// Shell policy: environment and working directory.
    pub shell: ShellPolicy,
    /// Resource limits: guards, not grants — safe values even when omitted.
    pub limits: Limits,
}

impl Policy {
    /// Validates invariants that would otherwise silently weaken deny-by-default.
    ///
    /// # Errors
    ///
    /// Returns [`PolicyError`] for a zero timeout, a relative path entry, or a
    /// protected name that is empty or contains a separator.
    pub fn validate(&self) -> Result<(), PolicyError> {
        if self.limits.timeout.is_zero() {
            return Err(PolicyError::ZeroTimeout);
        }
        for entry in &self.fs.entries {
            if !entry.path.is_absolute() {
                return Err(PolicyError::RelativeEntry(entry.path.clone()));
            }
        }
        for name in &self.fs.protected {
            if name.is_empty() || name.contains('/') {
                return Err(PolicyError::InvalidProtectedName(name.clone()));
            }
        }
        Ok(())
    }
}

/// A policy input that fails closed.
#[derive(Debug, Clone, PartialEq, Eq, ThisError)]
pub enum PolicyError {
    /// A zero wall-clock timeout would kill every command before it runs.
    #[error("the wall-clock timeout must be greater than zero")]
    ZeroTimeout,
    /// A path entry must be an absolute host path.
    #[error("path entry {0:?} must be absolute")]
    RelativeEntry(PathBuf),
    /// A protected name must be one path component, e.g. `.git`.
    #[error("protected name {0:?} must be a single non-empty path component")]
    InvalidProtectedName(String),
}

/// Filesystem policy for the OS confinement profile.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FsPolicy {
    /// Path entries, evaluated with the precedence `deny > write > read`.
    ///
    /// A `read` or `write` entry covers a directory (and everything under it)
    /// or a single file; `deny` removes access wherever it matches. A `write`
    /// entry is what the OS profile grants write access to; reads are granted
    /// broadly by the profile (see `docs/sandbox.md`) and narrowed by `deny`.
    pub entries: Vec<FsEntry>,
    /// Names fixed read-only inside any `write` root. Defaults to `.git` and
    /// `.agents`, so a command cannot rewrite the repository history it is
    /// diffed against or its own instructions.
    pub protected: Vec<String>,
}

impl Default for FsPolicy {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            protected: vec![String::from(".git"), String::from(".agents")],
        }
    }
}

/// One filesystem path rule.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FsEntry {
    /// The host path the rule applies to: a directory or a single file.
    pub path: PathBuf,
    /// The access the rule grants or removes.
    pub access: Access,
}

/// The access an [`FsEntry`] grants or removes.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Access {
    /// Read access; reads are granted broadly and this narrows from the top.
    Read,
    /// Write access; the only path that can reach the host writable.
    Write,
    /// No access, overriding any matching `read` or `write`.
    Deny,
}

/// Shell policy for commands executed through the sandbox.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ShellPolicy {
    /// The environment handed to commands, verbatim. The host environ is
    /// never inherited.
    pub env: EnvAllowlist,
    /// Host working directory for spawned commands.
    pub workdir: PathBuf,
}

/// The environment handed to commands: literal variables, never derived from
/// the host environ.
pub type EnvAllowlist = Vec<EnvVar>;

/// One literal environment variable.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvVar {
    /// Variable name, e.g. `CARGO_HOME`.
    pub name: String,
    /// Variable value passed to the command as-is.
    pub value: String,
}

/// Resource limits guarding against runaway commands.
///
/// There is deliberately no memory or byte cap: macOS has no mechanism to
/// enforce one, and an unenforced field would be a false promise. See
/// `docs/sandbox.md`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    /// Wall-clock timeout per command.
    #[serde(with = "duration_seconds")]
    pub timeout: Duration,
    /// Cap on captured command output bytes.
    pub max_output_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(60),
            max_output_bytes: 1024 * 1024,
        }
    }
}

/// Serializes `Duration` as whole seconds, which is the policy granularity.
mod duration_seconds {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::time::Duration;

    pub fn serialize<S: Serializer>(
        duration: &Duration,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        u64::serialize(&duration.as_secs(), serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Duration, D::Error> {
        Ok(Duration::from_secs(u64::deserialize(deserializer)?))
    }
}

#[cfg(test)]
mod tests {
    use super::{Access, EnvVar, FsEntry, FsPolicy, Limits, Policy, PolicyError, ShellPolicy};
    use serde_json::{Value, from_value, json, to_value};
    use std::path::PathBuf;
    use std::time::Duration;

    #[test]
    fn default_policy_is_fully_inert() {
        let policy = Policy::default();

        assert!(policy.fs.entries.is_empty());
        assert!(policy.shell.env.is_empty());
        assert_eq!(policy.shell.workdir, PathBuf::new());
    }

    #[test]
    fn default_protected_names_cover_repo_metadata() {
        assert_eq!(
            FsPolicy::default().protected,
            [String::from(".git"), String::from(".agents")]
        );
    }

    #[test]
    fn default_limits_are_safe_when_omitted() {
        let limits = Limits::default();

        assert_eq!(limits.timeout, Duration::from_secs(60));
        assert_eq!(limits.max_output_bytes, 1024 * 1024);
    }

    #[test]
    fn missing_fields_fill_with_inert_defaults() {
        let policy = from_value::<Policy>(json!({}))
            .expect("an empty JSON object is a valid, fully inert policy");

        assert_eq!(policy.fs.entries, Vec::new());
        assert_eq!(policy.limits, Limits::default());
    }

    #[test]
    fn policy_round_trips_through_json() {
        let policy = Policy {
            fs: FsPolicy {
                entries: vec![
                    FsEntry {
                        path: PathBuf::from("/repo"),
                        access: Access::Write,
                    },
                    FsEntry {
                        path: PathBuf::from("/home/dev/.ssh"),
                        access: Access::Deny,
                    },
                ],
                protected: vec![String::from(".git")],
            },
            shell: ShellPolicy {
                env: vec![EnvVar {
                    name: String::from("CARGO_HOME"),
                    value: String::from("/scratch/cargo"),
                }],
                workdir: PathBuf::from("/repo"),
            },
            limits: Limits {
                timeout: Duration::from_secs(30),
                max_output_bytes: 4096,
            },
        };

        let round_tripped: Policy = from_value(to_value(&policy).expect("serializable policy"))
            .expect("deserializable policy");

        assert_eq!(round_tripped, policy);
    }

    #[test]
    fn entry_serializes_with_snake_case_access() {
        let entry = FsEntry {
            path: PathBuf::from("/repo"),
            access: Access::Write,
        };

        assert_eq!(
            to_value(entry).expect("serializable entry"),
            json!({ "path": "/repo", "access": "write" })
        );
    }

    #[test]
    fn limits_serialize_timeout_as_whole_seconds() {
        let limits = Limits {
            timeout: Duration::from_secs(30),
            ..Limits::default()
        };

        let value = to_value(limits).expect("serializable limits");

        let Value::Object(fields) = value else {
            panic!("limits serialize to a JSON object");
        };
        assert_eq!(fields.get("timeout"), Some(&json!(30)));
    }

    #[test]
    fn validate_rejects_a_zero_timeout() {
        let zero = Policy {
            limits: Limits {
                timeout: Duration::ZERO,
                ..Limits::default()
            },
            ..Policy::default()
        };

        assert_eq!(zero.validate(), Err(PolicyError::ZeroTimeout));
    }

    #[test]
    fn validate_rejects_relative_entries_and_bad_protected_names() {
        let relative = Policy {
            fs: FsPolicy {
                entries: vec![FsEntry {
                    path: PathBuf::from("repo"),
                    access: Access::Read,
                }],
                ..FsPolicy::default()
            },
            ..Policy::default()
        };
        assert_eq!(
            relative.validate(),
            Err(PolicyError::RelativeEntry(PathBuf::from("repo")))
        );

        let nested = Policy {
            fs: FsPolicy {
                protected: vec![String::from(".git/hooks")],
                ..FsPolicy::default()
            },
            ..Policy::default()
        };
        assert_eq!(
            nested.validate(),
            Err(PolicyError::InvalidProtectedName(String::from(
                ".git/hooks"
            )))
        );
    }

    #[test]
    fn a_sound_default_policy_validates() {
        assert_eq!(Policy::default().validate(), Ok(()));
    }

    #[test]
    fn unknown_policy_fields_are_rejected() {
        assert!(from_value::<Policy>(json!({ "shells": {} })).is_err());
        assert!(from_value::<ShellPolicy>(json!({ "allowed": [] })).is_err());
        assert!(from_value::<Limits>(json!({ "timeouts": 30 })).is_err());
        assert!(from_value::<EnvVar>(json!({ "name": "A", "value": "b", "extra": 1 })).is_err());
    }
}
