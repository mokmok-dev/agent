//! The `filesystem` layer: rendering a [`Policy`](crate::policy::Policy) into the
//! operating system's confinement mechanism, and choosing which mechanism a host
//! can actually enforce.
//!
//! Milestone 2 implements the Linux **bubblewrap** backend: [`bwrap::render`]
//! produces the argument list, and [`Backend::detect`] chooses it only when a
//! probe shows that this host can build a namespace. A host where bubblewrap
//! cannot build one is [`Backend::Unsupported`], and the caller must refuse to
//! spawn rather than run the command unconfined. The Landlock fallback and the
//! macOS Seatbelt profile are later milestones; see `docs/sandbox/filesystem.md`.

mod bwrap;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

pub use bwrap::render;

/// Why a policy could not be rendered, or a command could not be confined.
#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    /// A `write` entry named a path that does not exist.
    ///
    /// bubblewrap cannot bind a path that is not there, and skipping the entry
    /// would silently make the root read-only instead of writable.
    #[error("write root `{path}` does not exist")]
    MissingWriteRoot {
        /// The missing path.
        path: PathBuf,
    },
    /// A `deny` entry named a path that does not exist.
    ///
    /// A missing deny target cannot be masked, and skipping it would silently
    /// leave the path unprotected.
    #[error("`deny` target `{path}` does not exist")]
    MissingDenyTarget {
        /// The missing path.
        path: PathBuf,
    },
    /// A socket the policy grants does not exist.
    ///
    /// A missing socket cannot be bound into the command's namespace, and
    /// skipping it would silently grant less than the policy declared. The
    /// egress proxy's socket and every `network.unix_sockets` entry take this
    /// path.
    #[error("socket `{path}` does not exist")]
    MissingSocket {
        /// The missing path.
        path: PathBuf,
    },
    /// The scratch directory lies inside a `deny` entry.
    ///
    /// The scratch is the command's `TMPDIR`, so a `deny` over it would remove
    /// the one writable place a command is guaranteed; the policy is rejected
    /// rather than run with a `TMPDIR` the command cannot use.
    #[error("the scratch directory `{scratch}` is inside a `deny` entry `{deny}`")]
    ScratchDenied {
        /// The scratch directory.
        scratch: PathBuf,
        /// The `deny` entry that covers it.
        deny: PathBuf,
    },
    /// A socket the policy grants lies inside a `deny` entry.
    ///
    /// The socket must be mounted into the command's namespace for the command
    /// to reach it; a `deny` over it (or its parent directory) would make
    /// bubblewrap unable to create the mountpoint, so the policy is rejected
    /// rather than run with a broken grant.
    #[error("the socket `{socket}` is inside a `deny` entry `{deny}`")]
    SocketDenied {
        /// The granted socket path.
        socket: PathBuf,
        /// The `deny` entry that covers it.
        deny: PathBuf,
    },
}

/// The confinement mechanism a host can enforce.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Backend {
    /// Linux bubblewrap, usable here because it can build a namespace.
    Bubblewrap {
        /// The resolved `bwrap` program.
        program: PathBuf,
    },
    /// No mechanism this build can enforce: the caller must refuse to spawn.
    Unsupported {
        /// Why no backend was selected, for the operator.
        reason: BackendError,
    },
}

/// Why no backend was selected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BackendError {
    /// No `bwrap` on `PATH`.
    #[error("no `bwrap` on PATH")]
    BubblewrapNotInstalled,
    /// A `bwrap` was found, but it could not build the namespace a confinement
    /// needs here: an unprivileged user namespace is disabled, or the daemon
    /// runs under `no_new_privs` in a nested container.
    #[error("`bwrap` at `{program}` cannot build a namespace here: {detail}")]
    BubblewrapCannotBuildNamespace {
        /// The `bwrap` that was probed.
        program: PathBuf,
        /// The probe's diagnostic.
        detail: String,
    },
}

impl Backend {
    /// Detect the backend a host can enforce, by capability rather than presence.
    ///
    /// `search_path` is the `PATH` to resolve `bwrap` against, and `reject_under`
    /// is the policy's writable root: a `bwrap` inside it is rejected, because a
    /// repository must not supply the very program that confines it.
    ///
    /// bubblewrap is probed by running a throwaway `--unshare-all`. A host that
    /// installs bubblewrap but forbids unprivileged user namespaces would
    /// otherwise fail at spawn, after a session was announced.
    #[must_use]
    pub fn detect(
        search_path: &str,
        reject_under: &[PathBuf],
    ) -> Self {
        let Some(program) = which("bwrap", search_path) else {
            return Self::Unsupported {
                reason: BackendError::BubblewrapNotInstalled,
            };
        };

        // A `bwrap` inside a policy write root would let a confined command
        // supply the binary that confines it, so it is refused even when present.
        if reject_under.iter().any(|root| program.starts_with(root)) {
            return Self::Unsupported {
                reason: BackendError::BubblewrapCannotBuildNamespace {
                    program,
                    detail: "the program is inside a policy write root".to_owned(),
                },
            };
        }

        match probe_namespace(&program) {
            Ok(()) => Self::Bubblewrap { program },
            Err(detail) => Self::Unsupported {
                reason: BackendError::BubblewrapCannotBuildNamespace { program, detail },
            },
        }
    }

    /// Whether a backend can confine a command.
    #[must_use]
    pub const fn is_supported(&self) -> bool {
        matches!(self, Self::Bubblewrap { .. })
    }

    /// The argument list for `policy`, ending before the `--` separator, or
    /// `None` when no backend is supported.
    ///
    /// # Errors
    ///
    /// Returns [`RenderError`] when the policy names a path that does not exist.
    pub fn render(
        &self,
        policy: &crate::policy::Policy,
        scratch: &Path,
    ) -> Result<Option<Vec<OsString>>, RenderError> {
        match self {
            Self::Bubblewrap { .. } => bwrap::render(policy, scratch).map(Some),
            Self::Unsupported { .. } => Ok(None),
        }
    }
}

/// Run a throwaway `bwrap --unshare-all` to see whether a namespace can be built.
fn probe_namespace(program: &Path) -> Result<(), String> {
    let output = Command::new(program)
        .args([
            "--unshare-all",
            "--die-with-parent",
            "--ro-bind",
            "/",
            "/",
            "true",
        ])
        .output()
        .map_err(|error| format!("could not run the probe: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        let detail = String::from_utf8_lossy(&output.stderr);
        Err(format!("the probe failed: {}", detail.trim()))
    }
}

/// Resolve `name` on `search_path`, the way a shell would.
///
/// An empty entry is skipped. POSIX gives it the "current directory" meaning, so
/// honouring it would let a confined command plant a `bwrap` in its working
/// directory and be confined by that; the filter keeps trust out of the cwd.
fn which(
    name: &str,
    search_path: &str,
) -> Option<PathBuf> {
    search_dirs(search_path)
        .map(|dir| Path::new(dir).join(name))
        .find(|candidate| candidate.is_file())
}

/// The non-empty directories of a `PATH`-style string, in order.
fn search_dirs(search_path: &str) -> impl Iterator<Item = &str> {
    search_path.split(':').filter(|dir| !dir.is_empty())
}

#[cfg(test)]
mod tests {
    // Tests for backend detection. The namespace probe is skipped when this host
    // cannot build one, matching the rule that a host where the probe fails
    // would not select bubblewrap either.

    use super::*;

    /// The host's `PATH`.
    fn path() -> String {
        std::env::var("PATH").unwrap_or_default()
    }

    #[test]
    fn a_bubblewrap_outside_every_write_root_is_usable_when_the_probe_passes() {
        // Detection on the real host: either bwrap is absent (Unsupported, not
        // installed) or it is present. If present, assert the probe agrees with
        // detection: a selected backend means the probe passed.
        let backend = Backend::detect(&path(), &[]);
        match &backend {
            Backend::Bubblewrap { program } => {
                assert!(program.is_file());
                assert!(probe_namespace(program).is_ok());
            },
            Backend::Unsupported { reason } => match reason {
                BackendError::BubblewrapNotInstalled => {
                    assert!(which("bwrap", &path()).is_none());
                },
                BackendError::BubblewrapCannotBuildNamespace { .. } => {
                    // A present-but-unusable bwrap is a valid host state.
                },
            },
        }
    }

    #[test]
    fn a_bubblewrap_inside_a_write_root_is_refused() {
        // Find a real bwrap, then pretend the host's root is a write root.
        if which("bwrap", &path()).is_none() {
            return;
        }
        let backend = Backend::detect(&path(), &[PathBuf::from("/")]);
        assert!(
            !backend.is_supported(),
            "a bwrap under a write root must not be selected"
        );
    }

    #[test]
    fn an_empty_search_path_finds_nothing() {
        let backend = Backend::detect("", &[]);
        assert_eq!(
            backend,
            Backend::Unsupported {
                reason: BackendError::BubblewrapNotInstalled
            }
        );
    }

    #[test]
    fn which_ignores_empty_path_entries() {
        // `::` is three empty entries; none is searched. A real entry that does
        // not contain the name also misses.
        assert!(which("bwrap", "::").is_none());
        assert!(which("bwrap", "/nonexistent-dir").is_none());
        assert_eq!(search_dirs("::").count(), 0);
        assert_eq!(search_dirs("/a::/b:").collect::<Vec<_>>(), ["/a", "/b"]);
    }

    #[test]
    fn which_finds_a_file_in_a_listed_directory() {
        // Use a real directory with a known file to exercise the hit path, so a
        // filter or join that drops the entry is caught.
        let dir = std::env::temp_dir();
        let candidate = dir.join("sandbox-which-probe");
        std::fs::write(&candidate, b"x").expect("writes the probe");
        let found = which("sandbox-which-probe", &dir.to_string_lossy());
        assert_eq!(found.as_deref(), Some(candidate.as_path()));
        let _ = std::fs::remove_file(&candidate);
    }

    #[test]
    fn is_supported_reflects_the_variant() {
        let supported = Backend::Bubblewrap {
            program: PathBuf::from("/bin/bwrap"),
        };
        assert!(supported.is_supported());

        let unsupported = Backend::Unsupported {
            reason: BackendError::BubblewrapNotInstalled,
        };
        assert!(!unsupported.is_supported());
    }

    #[test]
    fn an_unsupported_backend_renders_nothing() {
        let backend = Backend::Unsupported {
            reason: BackendError::BubblewrapNotInstalled,
        };
        let policy = crate::policy::Policy::default();
        assert!(
            backend
                .render(&policy, Path::new("/tmp"))
                .expect("renders")
                .is_none()
        );
    }
}
