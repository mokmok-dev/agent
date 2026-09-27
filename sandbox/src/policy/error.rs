//! The errors a [`Policy`](super::Policy) can fail validation with.
//!
//! Every variant is a construction failure: a policy that would be unsatisfiable
//! or unsafe never reaches the kernel. See `docs/sandbox/filesystem.md`.

use std::path::PathBuf;

/// A violation of a [`Policy`](super::Policy)'s invariants.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum InvalidPolicy {
    /// A path that must be absolute is not.
    #[error("path `{path}` is not absolute")]
    NotAbsolute {
        /// The offending path.
        path: PathBuf,
    },
    /// A path contains a `..` component, so it can escape the root it appears to
    /// name and does not identify a stable location.
    #[error("path `{path}` is not lexically normalized")]
    NotNormalized {
        /// The offending path.
        path: PathBuf,
    },
    /// The working directory is not writable under the policy.
    ///
    /// This covers a workdir that no `write` entry covers, one a `deny` covers,
    /// and one inside a protected name: all three leave it effectively
    /// read-only, and a read-only workdir breaks every command.
    #[error("working directory `{workdir}` is not writable")]
    WorkdirNotWritable {
        /// The working directory.
        workdir: PathBuf,
    },
    /// A `deny` entry covers a `write` root, making that root unconditionally
    /// unwritable.
    #[error("deny entry `{deny}` covers write root `{root}`")]
    DenyCoversWriteRoot {
        /// The deny entry.
        deny: PathBuf,
        /// The write root it covers.
        root: PathBuf,
    },
    /// A protected name is not a single, normal path component.
    #[error("protected name `{name}` is not a single normal path component")]
    InvalidProtectedName {
        /// The offending name.
        name: String,
    },
    /// An environment variable name is empty or contains `=`.
    #[error("environment variable name `{name}` is invalid")]
    InvalidEnvName {
        /// The offending name.
        name: String,
    },
    /// An environment variable name or value contains a NUL byte.
    #[error("environment variable `{name}` contains a NUL byte")]
    InvalidEnvValue {
        /// The variable whose name or value is invalid.
        name: String,
    },
    /// The wall-clock timeout is zero, which would kill every command at once.
    #[error("the wall-clock timeout must be non-zero")]
    ZeroTimeout,
    /// The output cap is zero, which would let a command produce no output at
    /// all.
    #[error("the output cap must be non-zero")]
    ZeroOutputCap,
    /// An egress destination has an empty host.
    #[error("egress destination has an empty host")]
    EmptyHost,
    /// An egress destination host contains whitespace or a NUL byte.
    #[error("egress destination host `{host}` is invalid")]
    InvalidHost {
        /// The offending host.
        host: String,
    },
    /// An egress destination uses port zero, which names no service.
    #[error("egress destination `{host}` uses port zero")]
    ZeroPort {
        /// The host whose port is zero.
        host: String,
    },
    /// The proxy's forwarded port is zero, which names no port.
    #[error("the proxy port must be non-zero")]
    ZeroProxyPort,
}
