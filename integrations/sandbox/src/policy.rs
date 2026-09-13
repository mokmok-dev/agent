//! The sandbox policy model.
//!
//! A [`Policy`] is deny-by-default in every domain: the zero value mounts
//! nothing, allows no command, allows no network access, and imposes safe
//! resource limits. Policies serialize into JSON so they can arrive as event
//! data; every field defaults to its inert value when absent.

use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// The four-domain deny-by-default configuration a sandbox runs under.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Policy {
    /// Filesystem policy: mounts, refused and hidden path globs, byte caps.
    pub fs: FsPolicy,
    /// Shell policy: allowed command prefixes, environment, working directory.
    pub shell: ShellPolicy,
    /// Network policy: nothing to configure in this version.
    pub network: NetworkPolicy,
    /// Resource limits: guards, not grants — safe values even when omitted.
    pub limits: Limits,
}

/// Filesystem policy for the virtual filesystem.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FsPolicy {
    /// The mounts that make up the virtual filesystem; unmounted paths are
    /// absent.
    pub mounts: Vec<Mount>,
    /// Access denied wherever the pattern matches, e.g. `.env`, `*.pem`,
    /// `.git/**`.
    pub refuse: Vec<Pattern>,
    /// Paths that appear absent, including in directory listings.
    pub hide: Vec<Pattern>,
    /// Cap on the total bytes written through the sandbox.
    pub max_total_bytes: Option<u64>,
    /// Cap on the bytes of a single file written through the sandbox.
    pub max_file_bytes: Option<u64>,
}

/// A mount binds a virtual location to a backing source.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Mount {
    /// The virtual mount point, sandbox-absolute (e.g. `/work`). Mounting at
    /// `/` backs the whole virtual root.
    pub at: PathBuf,
    /// What backs the mount.
    pub source: MountSource,
}

/// What backs a [`Mount`].
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MountSource {
    /// Fully in-memory and byte-accounted; the default and the safest backing.
    Mem,
    /// A host directory, read-only through the sandbox.
    ReadOnly {
        /// The host directory the mount is rooted at.
        host: PathBuf,
    },
    /// A host directory, writable through the sandbox: the only write path to
    /// the host.
    ReadWrite {
        /// The host directory the mount is rooted at.
        host: PathBuf,
    },
    /// A host directory with copy-on-write: reads fall through to the host,
    /// writes stay in memory. The host side is never written through the
    /// sandbox; in layer 1 it is additionally read-only at the OS level.
    Overlay {
        /// The host directory the mount is rooted at.
        host: PathBuf,
    },
}

/// A glob pattern evaluated against virtual paths.
///
/// Patterns use deny-anywhere semantics: a pattern matches a path when it
/// matches the full path or any path suffix starting after a separator, so
/// `.env` denies `/work/.env` and `/work/sub/.env`, and `.git/**` denies
/// everything under any `.git` directory.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Pattern(String);

impl Pattern {
    /// Creates a pattern from a glob string.
    #[must_use]
    pub fn new(pattern: impl Into<String>) -> Self {
        Self(pattern.into())
    }

    /// The glob string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Shell policy for commands executed through the sandbox.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ShellPolicy {
    /// Allowed command prefixes. Deny-by-default: an empty list means no
    /// command runs.
    pub allow: Vec<CommandPrefix>,
    /// The environment handed to commands, verbatim. The host environ is
    /// never inherited.
    pub env: EnvAllowlist,
    /// Host working directory for spawned commands.
    pub workdir: PathBuf,
}

/// An allowed command prefix: the command's tokens must start with exactly
/// these tokens.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CommandPrefix(String);

impl CommandPrefix {
    /// Creates a prefix from its token list, e.g. `cargo test`.
    #[must_use]
    pub fn new(prefix: impl Into<String>) -> Self {
        Self(prefix.into())
    }

    /// The prefix string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The environment handed to commands: literal variables, never derived from
/// the host environ.
pub type EnvAllowlist = Vec<EnvVar>;

/// One literal environment variable.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct EnvVar {
    /// Variable name, e.g. `CARGO_HOME`.
    pub name: String,
    /// Variable value passed to the command as-is.
    pub value: String,
}

/// Network policy.
///
/// This version has no configurable surface: spawned commands are denied
/// network access at the OS level as part of the deny-default confinement
/// profile, and no in-process consumer performs network access yet.
/// Per-host rules and the SSRF guard arrive with the first consumer; see
/// `docs/sandbox.md`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct NetworkPolicy {}

/// Resource limits guarding against runaway commands.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Limits {
    /// Wall-clock timeout per command.
    #[serde(with = "duration_seconds")]
    pub timeout: Duration,
    /// Command count per sandbox: a fork-bomb and runaway-loop guard.
    pub max_command_count: u32,
    /// Cap on captured command output bytes.
    pub max_output_bytes: u64,
    /// Best-effort memory cap. Configuration-only in this version: there is
    /// no hard memory ceiling for spawned commands on macOS, and enforcing
    /// rlimits is out of scope; see `docs/sandbox.md`.
    pub max_memory_bytes: Option<u64>,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(60),
            max_command_count: 1_000,
            max_output_bytes: 1024 * 1024,
            max_memory_bytes: None,
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
    use super::{
        CommandPrefix, EnvVar, Limits, Mount, MountSource, NetworkPolicy, Pattern, Policy,
        ShellPolicy,
    };
    use serde_json::{Value, from_value, json, to_value};
    use std::path::PathBuf;
    use std::time::Duration;

    #[test]
    fn default_policy_is_fully_inert() {
        let policy = Policy::default();

        assert!(policy.fs.mounts.is_empty());
        assert!(policy.fs.refuse.is_empty());
        assert!(policy.fs.hide.is_empty());
        assert_eq!(policy.fs.max_total_bytes, None);
        assert_eq!(policy.fs.max_file_bytes, None);
        assert!(policy.shell.allow.is_empty());
        assert!(policy.shell.env.is_empty());
        assert_eq!(policy.shell.workdir, PathBuf::new());
        assert_eq!(policy.network, NetworkPolicy {});
    }

    #[test]
    fn default_limits_are_safe_when_omitted() {
        let limits = Limits::default();

        assert_eq!(limits.timeout, Duration::from_secs(60));
        assert_eq!(limits.max_command_count, 1_000);
        assert_eq!(limits.max_output_bytes, 1024 * 1024);
        assert_eq!(limits.max_memory_bytes, None);
    }

    #[test]
    fn missing_fields_fill_with_inert_defaults() {
        let policy = from_value::<Policy>(json!({}))
            .expect("an empty JSON object is a valid, fully inert policy");

        assert_eq!(policy, Policy::default());
    }

    #[test]
    fn policy_round_trips_through_json() {
        let policy = Policy {
            fs: super::FsPolicy {
                mounts: vec![Mount {
                    at: PathBuf::from("/work"),
                    source: MountSource::Overlay {
                        host: PathBuf::from("/repo"),
                    },
                }],
                refuse: vec![Pattern::new(".env"), Pattern::new("*.pem")],
                hide: vec![Pattern::new(".git/**")],
                max_total_bytes: Some(1 << 30),
                max_file_bytes: Some(1 << 20),
            },
            shell: ShellPolicy {
                allow: vec![CommandPrefix::new("cargo test")],
                env: vec![EnvVar {
                    name: String::from("CARGO_HOME"),
                    value: String::from("/scratch/cargo"),
                }],
                workdir: PathBuf::from("/repo"),
            },
            network: NetworkPolicy {},
            limits: Limits {
                timeout: Duration::from_secs(30),
                max_command_count: 100,
                max_output_bytes: 4096,
                max_memory_bytes: Some(1 << 28),
            },
        };

        let round_tripped: Policy = from_value(to_value(&policy).expect("serializable policy"))
            .expect("deserializable policy");

        assert_eq!(round_tripped, policy);
    }

    #[test]
    fn mount_serializes_with_snake_case_tagging() {
        let mount = Mount {
            at: PathBuf::from("/work"),
            source: MountSource::ReadWrite {
                host: PathBuf::from("/repo"),
            },
        };

        let value = to_value(mount).expect("serializable mount");

        assert_eq!(
            value,
            json!({ "at": "/work", "source": { "read_write": { "host": "/repo" } } })
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
}
