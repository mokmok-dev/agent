//! The sandbox: the confinement layer through which an agent drives shell
//! commands. Every command runs under an OS-enforced policy — never directly
//! against the host — and every permission decision and OS denial is durably
//! appended to the event log as a `CloudEvent`.
//!
//! The crate is deny-by-default for what a command can *do*: the [`Policy`]
//! zero value mounts nothing, allows no command, and opens no listener. Reads
//! are narrowed only by [`FsPolicy::deny_read`](policy::FsPolicy::deny_read);
//! network egress is target-default-deny (the current macOS profile still opens
//! outbound — see [`NetworkPolicy`] and `docs/sandbox.md`). The VFS is a
//! layer-2 construct for a future in-process executor and does not confine
//! spawned commands; the executor renders the policy into an OS profile, which
//! is the boundary.
//!
//! # Example
//!
//! A sandbox whose agent can read a repository through a copy-on-write
//! overlay and run `cargo test` in it:
//!
//! ```
//! use agentd_events::EventLog;
//! use agentd_sandbox::{
//!     CommandPrefix, FsPolicy, Mount, MountSource, Policy, Sandbox, ShellPolicy,
//! };
//! use std::path::PathBuf;
//!
//! # fn main() -> Result<(), agentd_sandbox::SandboxError> {
//! # let repo = std::env::temp_dir().join("agentd-sandbox-doc-test");
//! # std::fs::create_dir_all(&repo).expect("repo dir");
//! let policy = Policy {
//!     fs: FsPolicy {
//!         mounts: vec![Mount {
//!             at: PathBuf::from("/work"),
//!             source: MountSource::Overlay { host: repo.clone() },
//!         }],
//!         ..FsPolicy::default()
//!     },
//!     shell: ShellPolicy {
//!         allow: vec![CommandPrefix::from("cargo test")],
//!         workdir: repo,
//!         ..ShellPolicy::default()
//!     },
//!     ..Policy::default()
//! };
//!
//! # fn build(policy: Policy) -> Result<Sandbox, agentd_sandbox::SandboxError> {
//! # let log_path = std::env::temp_dir().join("agentd-sandbox-doc-events.jsonl");
//! let log = EventLog::open(&log_path)?;
//! let sandbox = Sandbox::new(policy, log, "coder-1")?;
//! # let _ = std::fs::remove_file(&log_path);
//! # Ok(sandbox) }
//! # let sandbox = build(policy)?;
//! let _events = sandbox.log().subscribe();
//! # let _ = std::fs::remove_dir_all(std::env::temp_dir().join("agentd-sandbox-doc-test"));
//! # Ok(())
//! # }
//! ```
//!
//! See `docs/sandbox.md` for the design and its stated gaps.

mod error;
mod events;
mod executor;
mod policy;
mod sandbox;
#[cfg(target_os = "macos")]
mod seatbelt;
mod shell;
mod vfs;
mod vpath;

pub use error::SandboxError;
pub use events::{
    ACTION_EXEC, DECISION_AUTO, DECISION_PENDING, EXEC_COMPLETED, PERMISSION_DENIED,
    PERMISSION_GRANTED, PERMISSION_REQUESTED, RESOURCE_SHELL, exec_completed, permission_denied,
    permission_granted, permission_requested,
};
pub use executor::{ConfinedProcessExecutor, DenialReason, ExecResult, Executor};
pub use policy::{
    CommandPrefix, EnvAllowlist, EnvVar, FsPolicy, Limits, Mount, MountSource, NetworkPolicy,
    Pattern, Policy, PolicyError, ShellPolicy,
};
pub use sandbox::Sandbox;
pub use vfs::{DirEntry, Metadata, MountedVfs, Vfs};
pub use vpath::VPath;
