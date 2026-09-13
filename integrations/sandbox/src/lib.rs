//! The sandbox: the confinement layer through which an agent drives shell
//! commands. Every command runs against a virtual filesystem and a policy —
//! never directly against the host — and every permission decision is
//! published as a `CloudEvent` onto the event bus.
//!
//! The crate is deny-by-default in all three planes: the [`Policy`] zero value
//! mounts nothing, allows no command, and allows no network access; the VFS
//! implementations confine every path operation to their mounts; and the
//! executor translates what remains into an OS confinement profile.
//!
//! # Example
//!
//! A sandbox whose agent can read a repository through a copy-on-write
//! overlay and run `cargo test` in it:
//!
//! ```
//! use agentd_events::EventBus;
//! use agentd_integration_sandbox::{
//!     CommandPrefix, FsPolicy, Mount, MountSource, Policy, Sandbox, ShellPolicy,
//! };
//! use std::path::PathBuf;
//!
//! # fn main() -> Result<(), agentd_integration_sandbox::SandboxError> {
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
//! # fn build(policy: Policy) -> Result<Sandbox, agentd_integration_sandbox::SandboxError> {
//! let sandbox = Sandbox::new(policy, EventBus::default(), "coder-1")?;
//! # Ok(sandbox) }
//! # let sandbox = build(policy)?;
//! let _events = sandbox.bus().subscribe();
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
pub use executor::{DenialReason, ExecResult, Executor};
pub use policy::{
    CommandPrefix, EnvAllowlist, EnvVar, FsPolicy, Limits, Mount, MountSource, NetworkPolicy,
    Pattern, Policy, ShellPolicy,
};
pub use sandbox::Sandbox;
pub use vfs::{DirEntry, Metadata, MountedVfs, Vfs};
pub use vpath::VPath;
