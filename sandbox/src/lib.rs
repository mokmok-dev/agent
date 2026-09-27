//! Kernel-enforced confinement for the commands an agent runs.
//!
//! The sandbox denies filesystem writes and network egress by default, then
//! renders a [`policy::Policy`] into the operating system's isolation mechanism
//! (bubblewrap or Landlock on Linux, Seatbelt on macOS). The design is in
//! `docs/sandbox/`.
//!
//! Milestone 1 implements the **policy core**: the four domains, their
//! invariants, and the `deny > write > read` path precedence. It is a pure data
//! model that spawns nothing.
//!
//! Milestone 2 adds the **filesystem** layer: [`filesystem::Backend::detect`]
//! chooses a kernel mechanism by capability, and [`filesystem::render`] turns a
//! policy into the argument list of a bubblewrap invocation. The Landlock
//! fallback and the macOS Seatbelt profile are later milestones.
//!
//! # The precedence rule
//!
//! A path is writable only inside a `write` entry, and a `deny` nested in a
//! broader grant wins:
//!
//! ```
//! use sandbox::policy::{Access, FsEntry, FsPolicy, Policy, ShellPolicy};
//! use std::path::Path;
//!
//! let policy = Policy {
//!     fs: FsPolicy {
//!         entries: vec![
//!             FsEntry::write("/work"),
//!             FsEntry::deny("/work/secrets"),
//!         ],
//!         protected: FsPolicy::default().protected,
//!     },
//!     shell: ShellPolicy {
//!         env: Vec::new(),
//!         workdir: "/work".into(),
//!     },
//!     ..Policy::default()
//! };
//! policy.validate()?;
//!
//! assert_eq!(policy.fs.access_for(Path::new("/work/src/main.rs")), Access::Write);
//! assert_eq!(policy.fs.access_for(Path::new("/work/secrets/key")), Access::Deny);
//! // Reads are the broad host grant a `deny` narrows.
//! assert_eq!(policy.fs.access_for(Path::new("/etc/hosts")), Access::Read);
//! # Ok::<(), sandbox::policy::InvalidPolicy>(())
//! ```

pub mod executor;
pub mod filesystem;
pub mod policy;
