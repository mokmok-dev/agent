//! The `Policy`: the deny-by-default configuration a confined command runs under.
//!
//! A `Policy` has four domains. Its zero value grants no write and no network,
//! and reads are the broad host grant a `deny` narrows. [`Policy::default`] is a
//! starting point, **not** a runnable policy: it has no `write` entry, so its
//! working directory is not writable and [`Policy::validate`] rejects it. A
//! caller extends the default with the write root and workdir the command needs.
//! See `docs/sandbox/filesystem.md`.

mod access;
mod error;
mod fs;
mod limits;
mod network;
mod path;
mod shell;

use serde::{Deserialize, Serialize};

pub use access::Access;
pub use error::InvalidPolicy;
pub use fs::{DEFAULT_PROTECTED, FsEntry, FsPolicy};
pub use limits::{DEFAULT_MAX_OUTPUT_BYTES, DEFAULT_TIMEOUT_MILLIS, Limits};
pub use network::{HostPort, NetworkPolicy, ProxyGrant};
pub use shell::{EnvVar, ShellPolicy};

/// The complete confinement configuration for one command.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    /// Path entries, protected names, and their precedence.
    #[serde(default)]
    pub fs: FsPolicy,
    /// The environment and working directory.
    #[serde(default)]
    pub shell: ShellPolicy,
    /// Unix sockets and the egress proxy.
    #[serde(default)]
    pub network: NetworkPolicy,
    /// The wall-clock timeout and output cap.
    #[serde(default)]
    pub limits: Limits,
}

impl Policy {
    /// Validate the whole policy.
    ///
    /// Each domain is checked for its own shape, then the cross-domain rule: the
    /// working directory must be *effectively writable* under the entries. Every
    /// failure is a construction error, so nothing is silently dropped and
    /// nothing reaches the kernel unvalidated.
    ///
    /// # Errors
    ///
    /// Returns the first [`InvalidPolicy`] found.
    pub fn validate(&self) -> Result<(), InvalidPolicy> {
        self.fs.validate()?;
        self.shell.validate()?;
        self.network.validate()?;
        self.limits.validate()?;
        self.require_workdir_writable()
    }

    /// Require the working directory to be effectively writable.
    ///
    /// Using [`FsPolicy::access_for`] rather than "is inside a `write` entry"
    /// catches three cases at once: no `write` entry covers the workdir, a
    /// `deny` covers it, and a protected name such as `.git` caps it at read.
    /// All leave the workdir read-only, which breaks every command, so all are
    /// rejected.
    fn require_workdir_writable(&self) -> Result<(), InvalidPolicy> {
        let workdir = &self.shell.workdir;
        if self.fs.access_for(workdir) == Access::Write {
            Ok(())
        } else {
            Err(InvalidPolicy::WorkdirNotWritable {
                workdir: workdir.clone(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    // Tests for whole-policy validation: the cross-domain rule that the workdir
    // must be effectively writable, the non-zero default limits, and serde.

    use std::path::PathBuf;

    use super::*;

    /// A minimal valid policy: one write root, the workdir inside it.
    fn valid() -> Policy {
        Policy {
            fs: FsPolicy {
                entries: vec![FsEntry::write("/work")],
                protected: Vec::new(),
            },
            shell: ShellPolicy {
                env: Vec::new(),
                workdir: PathBuf::from("/work"),
            },
            ..Policy::default()
        }
    }

    #[test]
    fn a_minimal_valid_policy_validates() {
        assert!(valid().validate().is_ok());
    }

    #[test]
    fn the_default_policy_has_non_zero_limits() {
        // The derived zero would be a trap; the default must let a command run.
        let policy = Policy::default();
        assert_ne!(policy.limits.timeout_millis, 0);
        assert_ne!(policy.limits.max_output_bytes, 0);
    }

    #[test]
    fn the_default_policy_is_not_runnable() {
        // The default has no write entry, so its workdir is not writable and it
        // is rejected. This pins the "a default is a starting point, not a
        // runnable policy" claim.
        assert!(matches!(
            Policy::default().validate(),
            Err(InvalidPolicy::WorkdirNotWritable { .. })
        ));
    }

    #[test]
    fn a_workdir_outside_every_write_root_is_rejected() {
        let policy = Policy {
            shell: ShellPolicy {
                env: Vec::new(),
                workdir: PathBuf::from("/elsewhere"),
            },
            ..valid()
        };
        assert!(matches!(
            policy.validate(),
            Err(InvalidPolicy::WorkdirNotWritable { .. })
        ));
    }

    #[test]
    fn a_read_entry_cannot_narrow_a_broader_write_root() {
        // Precedence is `deny > write > read`, and `write` is more restrictive
        // than `read`, so a nested `read` is overridden by the write root. Only
        // a `deny` narrows a write.
        let policy = Policy {
            fs: FsPolicy {
                entries: vec![FsEntry::write("/work"), FsEntry::read("/work/ro")],
                protected: Vec::new(),
            },
            shell: ShellPolicy {
                env: Vec::new(),
                workdir: PathBuf::from("/work/ro"),
            },
            ..Policy::default()
        };
        assert_eq!(
            policy.fs.access_for(&PathBuf::from("/work/ro")),
            Access::Write
        );
        assert!(policy.validate().is_ok());
    }

    #[test]
    fn a_workdir_inside_a_protected_name_is_rejected() {
        // `.git` is protected, so a workdir there is read-only in effect.
        let policy = Policy {
            fs: FsPolicy {
                entries: vec![FsEntry::write("/work")],
                protected: vec![".git".to_owned()],
            },
            shell: ShellPolicy {
                env: Vec::new(),
                workdir: PathBuf::from("/work/.git"),
            },
            ..Policy::default()
        };
        assert!(matches!(
            policy.validate(),
            Err(InvalidPolicy::WorkdirNotWritable { .. })
        ));
    }

    #[test]
    fn a_nested_deny_over_the_workdir_is_rejected() {
        // A deny over a subdirectory of the write root is legal in itself, but
        // not when the workdir sits inside it.
        let policy = Policy {
            fs: FsPolicy {
                entries: vec![FsEntry::write("/work"), FsEntry::deny("/work/sub")],
                protected: Vec::new(),
            },
            shell: ShellPolicy {
                env: Vec::new(),
                workdir: PathBuf::from("/work/sub"),
            },
            ..Policy::default()
        };
        assert!(matches!(
            policy.validate(),
            Err(InvalidPolicy::WorkdirNotWritable { .. })
        ));
    }

    #[test]
    fn a_deny_that_misses_the_workdir_is_allowed() {
        let policy = Policy {
            fs: FsPolicy {
                entries: vec![FsEntry::write("/work"), FsEntry::deny("/work/secrets")],
                protected: Vec::new(),
            },
            ..valid()
        };
        assert!(policy.validate().is_ok());
    }

    #[test]
    fn a_policy_round_trips_through_json() {
        let policy = Policy {
            fs: FsPolicy {
                entries: vec![
                    FsEntry::write("/work"),
                    FsEntry::read("/data"),
                    FsEntry::deny("/etc/secrets"),
                ],
                protected: vec![".git".to_owned()],
            },
            shell: ShellPolicy {
                env: vec![EnvVar::new("PATH", "/usr/bin"), EnvVar::new("LANG", "C")],
                workdir: PathBuf::from("/work"),
            },
            network: NetworkPolicy {
                unix_sockets: vec![PathBuf::from("/run/daemon.sock")],
                loopback: true,
                proxy: Some(ProxyGrant {
                    port: 8080,
                    socket: Some(PathBuf::from("/run/egress.sock")),
                    egress: vec![HostPort::new("api.example.com", 443)],
                }),
            },
            limits: Limits {
                timeout_millis: 1500,
                max_output_bytes: 4096,
            },
        };

        let json = serde_json::to_string(&policy).expect("serializes");
        let parsed: Policy = serde_json::from_str(&json).expect("parses");
        assert_eq!(parsed, policy);
    }

    #[test]
    fn a_missing_domain_falls_back_to_its_default() {
        // A policy naming only `fs` still has default shell, network, and limits.
        let json = r#"{"fs":{"entries":[{"path":"/work","access":"write"}]}}"#;
        let policy: Policy = serde_json::from_str(json).expect("parses");
        assert_eq!(policy.shell, ShellPolicy::default());
        assert_eq!(policy.network, NetworkPolicy::default());
        assert_eq!(policy.limits, Limits::default());
        assert_eq!(policy.fs.protected, FsPolicy::default().protected);
    }

    #[test]
    fn an_unknown_field_is_rejected() {
        let json = r#"{"fs":{"entries":[],"surprise":true}}"#;
        assert!(serde_json::from_str::<Policy>(json).is_err());
    }
}
