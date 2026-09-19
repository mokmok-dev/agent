//! The sandbox: the confinement layer through which an agent drives shell
//! commands. Every command runs under an OS-enforced policy — never directly
//! against the host — and every permission decision is durably appended to the
//! event log as a `CloudEvent`.
//!
//! The crate is deny-by-default for what a command can *do*: the [`Policy`]
//! zero value grants no path access, runs no command, and opens no listener.
//! Network egress and ingress are denied outright except for the Unix domain
//! sockets in the policy's network domain; the filesystem policy is one list of
//! path entries (`Access::Read`/`Write`/`Deny`) that the executor renders into
//! an OS confinement profile — Seatbelt on macOS, bubblewrap on Linux with a
//! Landlock-plus-seccomp helper as the fallback. The executor renders from the
//! policy; the policy is the interface.
//!
//! # Example
//!
//! A sandbox whose agent can work in a repository:
//!
//! ```no_run
//! use agentd_events::EventLog;
//! use agentd_sandbox::{Access, FsEntry, FsPolicy, Policy, Sandbox, ShellPolicy};
//! use std::path::PathBuf;
//!
//! # fn main() -> Result<(), agentd_sandbox::SandboxError> {
//! let repo = PathBuf::from("/repo");
//! let policy = Policy {
//!     fs: FsPolicy {
//!         entries: vec![FsEntry {
//!             path: repo.clone(),
//!             access: Access::Write,
//!         }],
//!         ..FsPolicy::default()
//!     },
//!     shell: ShellPolicy {
//!         workdir: repo,
//!         ..ShellPolicy::default()
//!     },
//!     ..Policy::default()
//! };
//!
//! # let log_path = std::env::temp_dir().join("agentd-sandbox-doc-events.jsonl");
//! let log = EventLog::open(&log_path)?;
//! let sandbox = Sandbox::new(&policy, log, "coder-1")?;
//! let _events = sandbox.log().subscribe();
//! # Ok(())
//! # }
//! ```
//!
//! See `docs/sandbox.md` for the design and its stated gaps.

mod error;
mod events;
mod executor;
#[cfg(target_os = "linux")]
pub mod helper;
#[cfg(target_os = "linux")]
mod linux;
mod policy;
#[cfg(unix)]
mod process;
mod sandbox;
#[cfg(target_os = "macos")]
mod seatbelt;
mod violation;

pub use error::SandboxError;
pub use events::{
    ACTION_EXEC, DECISION_AUTO, DECISION_PENDING, EXEC_COMPLETED, PERMISSION_DENIED,
    PERMISSION_GRANTED, PERMISSION_REQUESTED, RESOURCE_SHELL, SESSION_EXITED, SESSION_STARTED,
    exec_completed, permission_denied, permission_granted, permission_requested, session_exited,
    session_started,
};
pub use executor::{ConfinedProcessExecutor, ExecResult, Executor, SpawnError};
pub use policy::{
    Access, EnvAllowlist, EnvVar, FsEntry, FsPolicy, Limits, NetworkPolicy, Policy, PolicyError,
    ShellPolicy,
};
pub use sandbox::{Approval, Sandbox, SandboxedProcess};
pub use violation::{
    VIOLATION_FILESYSTEM, VIOLATION_NETWORK, Violation, ViolationKind, ViolationReason,
    classify_violation, violation_event,
};
