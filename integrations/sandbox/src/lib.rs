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
//! See `docs/sandbox.md` for the design and its stated gaps.

mod error;
mod events;
mod executor;
mod policy;
mod sandbox;
mod shell;
mod vfs;
mod vpath;

pub use error::SandboxError;
pub use events::{
    ACTION_EXEC, DECISION_AUTO, DECISION_PENDING, EXEC_COMPLETED, PERMISSION_DENIED,
    PERMISSION_GRANTED, PERMISSION_REQUESTED, RESOURCE_SHELL, exec_completed, permission_decision,
    permission_requested,
};
pub use executor::{DenialReason, ExecResult, Executor};
pub use policy::{
    CommandPrefix, EnvAllowlist, EnvVar, FsPolicy, Limits, Mount, MountSource, NetworkPolicy,
    Pattern, Policy, ShellPolicy,
};
pub use sandbox::Sandbox;
pub use vfs::{DirEntry, Metadata, MountedVfs, Vfs};
pub use vpath::VPath;
