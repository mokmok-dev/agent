//! Resolving the daemon's own helper binaries on the trusted side.
//!
//! Egress runs through two binaries the sandbox renders into the command's init
//! argv: `sandbox-supervisor`, which runs as the sandbox's init, and
//! `egress-forward`, which the supervisor starts. The daemon resolves both
//! **before** the policy renders, applying the same trust rule the sandbox
//! applies to `bwrap`: a binary inside a policy write root is rejected, because a
//! workspace must not supply the program that confines it. See
//! `docs/sandbox/network.md`.
//!
//! Resolution order is the one the design names: an explicit override first, then
//! a sibling of the running daemon (installed next to it), then `PATH`.

use std::path::{Path, PathBuf};

/// The supervisor binary's name.
pub const SUPERVISOR: &str = "sandbox-supervisor";
/// The forwarder binary's name.
pub const FORWARDER: &str = "egress-forward";
/// The environment variable that overrides the supervisor's path.
pub const SUPERVISOR_OVERRIDE: &str = "AGENTD_SANDBOX_SUPERVISOR";
/// The environment variable that overrides the forwarder's path.
pub const FORWARDER_OVERRIDE: &str = "AGENTD_EGRESS_FORWARD";

/// The two helper binaries an egress grant needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EgressBinaries {
    /// The `sandbox-supervisor` binary.
    pub supervisor: PathBuf,
    /// The `egress-forward` binary.
    pub forwarder: PathBuf,
}

/// Resolve both binaries, or `None` when either cannot be found.
///
/// `daemon_exe` is the running daemon's path, whose directory is searched, and
/// `reject_under` is the workspace roots a candidate must not lie inside.
#[must_use]
pub fn resolve_egress(
    daemon_exe: &Path,
    search_path: &str,
    reject_under: &[PathBuf],
) -> Option<EgressBinaries> {
    Some(EgressBinaries {
        supervisor: resolve(
            SUPERVISOR,
            SUPERVISOR_OVERRIDE,
            daemon_exe,
            search_path,
            reject_under,
        )?,
        forwarder: resolve(
            FORWARDER,
            FORWARDER_OVERRIDE,
            daemon_exe,
            search_path,
            reject_under,
        )?,
    })
}

/// Resolve one helper binary.
///
/// The override variable comes first when set, then a sibling of `daemon_exe`,
/// then each `PATH` directory. An empty `PATH` entry is skipped, as in a shell
/// and for the same reason: it would search the current directory.
fn resolve(
    name: &str,
    override_var: &str,
    daemon_exe: &Path,
    search_path: &str,
    reject_under: &[PathBuf],
) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(path) = std::env::var(override_var) {
        candidates.push(PathBuf::from(path));
    }
    if let Some(dir) = daemon_exe.parent() {
        candidates.push(dir.join(name));
    }
    for dir in search_path.split(':').filter(|dir| !dir.is_empty()) {
        candidates.push(Path::new(dir).join(name));
    }

    candidates.into_iter().find(|candidate| {
        candidate.is_file() && !reject_under.iter().any(|root| candidate.starts_with(root))
    })
}

#[cfg(test)]
mod tests {
    // Tests for the resolution order and the trust rule, using a temp directory
    // as a stand-in for the daemon's own directory.

    use super::*;

    /// A temp directory removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("daemon-helper-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("creates the dir");
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Create a file so `is_file` succeeds.
    fn touch(path: &Path) {
        std::fs::write(path, b"#!/bin/sh\n").expect("writes the candidate");
    }

    #[test]
    fn a_sibling_of_the_daemon_is_found_before_path() {
        let tree = TempDir::new("sibling");
        let daemon = tree.0.join("agentd");
        touch(&daemon);
        let supervisor = tree.0.join(SUPERVISOR);
        touch(&supervisor);
        let forwarder = tree.0.join(FORWARDER);
        touch(&forwarder);

        let found = resolve_egress(&daemon, "/nonexistent", &[]).expect("both siblings resolve");
        assert_eq!(found.supervisor, supervisor);
        assert_eq!(found.forwarder, forwarder);
    }

    #[test]
    fn a_binary_inside_a_write_root_is_refused() {
        let tree = TempDir::new("reject");
        let workspace = tree.0.join("workspace");
        std::fs::create_dir_all(&workspace).expect("creates the workspace");
        let daemon = tree.0.join("agentd");
        touch(&daemon);
        // The helpers are inside the workspace, so they must not be selected.
        touch(&workspace.join(SUPERVISOR));
        touch(&workspace.join(FORWARDER));

        assert!(
            resolve_egress(&daemon, "/nonexistent", std::slice::from_ref(&workspace)).is_none(),
            "a helper inside a write root must not be selected"
        );
    }

    #[test]
    fn a_missing_forwarder_fails_the_whole_resolution() {
        // Egress needs both binaries; one missing means no egress, not a partial
        // grant.
        let tree = TempDir::new("partial");
        let daemon = tree.0.join("agentd");
        touch(&daemon);
        touch(&tree.0.join(SUPERVISOR));
        assert!(resolve_egress(&daemon, "/nonexistent", &[]).is_none());
    }

    #[test]
    fn path_is_searched_when_there_is_no_sibling() {
        let tree = TempDir::new("path");
        let bin = tree.0.join("bin");
        std::fs::create_dir_all(&bin).expect("creates bin");
        touch(&bin.join(SUPERVISOR));
        touch(&bin.join(FORWARDER));
        let daemon = tree.0.join("elsewhere/agentd");

        let found =
            resolve_egress(&daemon, &bin.to_string_lossy(), &[]).expect("resolves from PATH");
        assert_eq!(found.supervisor, bin.join(SUPERVISOR));
    }

    #[test]
    fn an_empty_path_entry_is_not_searched() {
        // `::` is three empty entries; none is searched.
        let tree = TempDir::new("empty-path");
        let daemon = tree.0.join("agentd");
        touch(&daemon);
        assert!(resolve_egress(&daemon, "::", &[]).is_none());
    }
}
