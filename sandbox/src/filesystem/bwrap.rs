//! Rendering a [`Policy`] into the arguments of a `bwrap` invocation.
//!
//! The shape is fixed by `docs/sandbox/filesystem.md`: a broad read grant, a
//! private `/dev`, one writable bind per `write` entry, protected names re-bound
//! read-only, and `deny` entries masked *after* the grants so they override
//! them. The network namespace is dropped, so the command reaches nothing but
//! its own loopback. Mounting the egress proxy's socket and starting the
//! forwarder are the supervisor's job, in a later milestone.
//!
//! The returned arguments end before the `--` separator. The caller appends
//! `-- <program> <args>` and runs the result.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::policy::{Access, Policy};

use super::RenderError;

/// Render `policy` into the arguments that follow `bwrap` on its command line.
///
/// `scratch` is a directory the caller created and will remove. It is mounted as
/// a private tmpfs and exported as `TMPDIR`, so a command has somewhere to write
/// temporary files without any of them reaching the host.
///
/// # Errors
///
/// Returns [`RenderError::MissingWriteRoot`] or
/// [`RenderError::MissingDenyTarget`] when a `write` or `deny` target does not
/// exist, because neither can be rendered faithfully and running without it
/// would silently widen or narrow the policy.
pub fn render(
    policy: &Policy,
    scratch: &Path,
) -> Result<Vec<OsString>, RenderError> {
    let mut args = Vec::new();

    // The broad read grant and a private /dev, which a confined command needs.
    push(&mut args, "--ro-bind");
    push(&mut args, "/");
    push(&mut args, "/");
    push(&mut args, "--dev");
    push(&mut args, "/dev");

    // One writable bind per `write` entry. The bind is the entry's own path, so
    // the command sees the host location at the same path.
    for entry in writes(policy) {
        require_exists(&entry.path, MissingKind::WriteRoot)?;
        push(&mut args, "--bind");
        push_path(&mut args, &entry.path);
        push_path(&mut args, &entry.path);
    }

    // Re-bind each protected name read-only, over the writable root that
    // contains it. A name that does not exist yet is left alone: bubblewrap
    // cannot create a read-only mountpoint that is not there, so a fresh `.git`
    // can still be made, which is a stated gap.
    for protected in protected_paths(policy) {
        push(&mut args, "--ro-bind");
        push_path(&mut args, &protected);
        push_path(&mut args, &protected);
    }

    // Mask each `deny` after the grants, so a mount over an earlier grant wins.
    for entry in denies(policy) {
        mask(&mut args, &entry.path)?;
    }

    // The scratch must not be inside a `deny`: it is the command's TMPDIR, and a
    // mask over it would leave the command with no writable temporary space.
    for entry in denies(policy) {
        if scratch.starts_with(&entry.path) {
            return Err(RenderError::ScratchDenied {
                scratch: scratch.to_path_buf(),
                deny: entry.path.clone(),
            });
        }
    }

    // The private TMPDIR. The mountpoint exists on the host and is emptied into
    // a tmpfs, so nothing the command writes there survives.
    push(&mut args, "--tmpfs");
    push_path(&mut args, scratch);

    // The egress proxy's socket, when the policy grants egress over a Unix
    // socket. The socket is a filesystem object that crosses the network
    // namespace, so it is mounted into the command's view. The parent directories
    // are created first with `--dir`, so the mount does not depend on the socket's
    // directory already existing in the command's view; then the socket itself is
    // bound **read-write**, which is what lets the command `connect` to it. A
    // `deny` over the socket or its parent is a construction error: bubblewrap
    // could not create the mountpoint over a mask.
    if let Some(socket) = egress_socket(policy) {
        for entry in denies(policy) {
            if socket.starts_with(&entry.path) {
                return Err(RenderError::SocketDenied {
                    socket: socket.to_path_buf(),
                    deny: entry.path.clone(),
                });
            }
        }
        require_exists(socket, MissingKind::EgressSocket)?;
        // The socket's parent directory is created explicitly, so the mount does
        // not depend on it already existing in the command's view (it might be
        // under the scratch tmpfs, for instance). `--dir` on an existing
        // directory is a no-op.
        if let Some(parent) = socket.parent()
            && !parent.as_os_str().is_empty()
        {
            push(&mut args, "--dir");
            push_path(&mut args, parent);
        }
        push(&mut args, "--bind");
        push_path(&mut args, socket);
        push_path(&mut args, socket);
    }

    // The environment is an allowlist: clear the host environment, then set
    // exactly what the policy names. `TMPDIR` is set last so the sandbox owns it.
    push(&mut args, "--clearenv");
    for var in &policy.shell.env {
        push(&mut args, "--setenv");
        push(&mut args, &var.name);
        push(&mut args, &var.value);
    }
    push(&mut args, "--setenv");
    push(&mut args, "TMPDIR");
    push_path(&mut args, scratch);

    // No namespace stays shared, and no process outlives the daemon. The latter
    // is what makes killing `bwrap` bring down the whole command tree.
    push(&mut args, "--unshare-all");
    push(&mut args, "--die-with-parent");

    // The working directory, which validation already required to be writable.
    push(&mut args, "--chdir");
    push_path(&mut args, &policy.shell.workdir);

    Ok(args)
}

/// The `write` entries.
fn writes(policy: &Policy) -> impl Iterator<Item = &crate::policy::FsEntry> {
    policy
        .fs
        .entries
        .iter()
        .filter(|e| e.access == Access::Write)
}

/// The `deny` entries.
fn denies(policy: &Policy) -> impl Iterator<Item = &crate::policy::FsEntry> {
    policy
        .fs
        .entries
        .iter()
        .filter(|e| e.access == Access::Deny)
}

/// The proxy's Unix socket, when egress is granted over one.
fn egress_socket(policy: &Policy) -> Option<&Path> {
    policy
        .network
        .proxy
        .as_ref()
        .and_then(|grant| grant.socket.as_deref())
}

/// The `<write-root>/<protected>` paths that exist, for a read-only re-bind.
fn protected_paths(policy: &Policy) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for root in writes(policy) {
        for name in &policy.fs.protected {
            let candidate = root.path.join(name);
            if candidate.exists() {
                paths.push(candidate);
            }
        }
    }
    paths
}

/// Mask `path` so it is neither readable nor writable.
///
/// A directory becomes an empty, read-only tmpfs: the tmpfs hides its contents
/// and `--remount-ro` stops the command writing into the replacement. A file
/// becomes a read-only null device. This is tighter than the design's plain
/// `--tmpfs` mask, which would leave a writable directory where a `deny` was
/// asked for.
fn mask(
    args: &mut Vec<OsString>,
    path: &Path,
) -> Result<(), RenderError> {
    require_exists(path, MissingKind::DenyTarget)?;
    // A directory becomes an empty read-only tmpfs; a file becomes the null
    // device bound read-only. Either way the target is unreadable and unwritable;
    // the target path is the shared tail of both.
    if path.is_dir() {
        push(args, "--tmpfs");
        push_path(args, path);
        push(args, "--remount-ro");
    } else {
        push(args, "--ro-bind");
        push(args, "/dev/null");
    }
    push_path(args, path);
    Ok(())
}

/// Require that `path` exists, mapping its absence to the right error.
fn require_exists(
    path: &Path,
    kind: MissingKind,
) -> Result<(), RenderError> {
    if path.exists() {
        return Ok(());
    }
    Err(match kind {
        MissingKind::WriteRoot => RenderError::MissingWriteRoot {
            path: path.to_path_buf(),
        },
        MissingKind::DenyTarget => RenderError::MissingDenyTarget {
            path: path.to_path_buf(),
        },
        MissingKind::EgressSocket => RenderError::MissingEgressSocket {
            path: path.to_path_buf(),
        },
    })
}

/// Which kind of required path is being checked.
#[derive(Debug, Clone, Copy)]
enum MissingKind {
    /// A `write` root.
    WriteRoot,
    /// A `deny` target.
    DenyTarget,
    /// The egress proxy's socket.
    EgressSocket,
}

/// Push a borrowed string argument.
fn push(
    args: &mut Vec<OsString>,
    arg: &str,
) {
    args.push(OsString::from(arg));
}

/// Push a borrowed path argument.
fn push_path(
    args: &mut Vec<OsString>,
    path: &Path,
) {
    args.push(path.as_os_str().to_os_string());
}

#[cfg(test)]
mod tests {
    // Tests for the bubblewrap argument renderer. The paths are real temp dirs,
    // because the renderer checks which `write` and `deny` targets exist.

    use std::fs;

    use super::*;
    use crate::policy::{FsEntry, FsPolicy, ShellPolicy};

    /// A temp tree with a write root, an existing protected name, a deny target,
    /// and a scratch dir. Removed on drop.
    struct Tree(PathBuf);

    impl Tree {
        fn new(tag: &str) -> Self {
            let root =
                std::env::temp_dir().join(format!("sandbox-bwrap-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(root.join("work/.git")).expect("creates the tree");
            fs::create_dir_all(root.join("work/secret")).expect("creates the tree");
            fs::write(root.join("work/secret/key"), b"x").expect("writes");
            fs::write(root.join("work/afile"), b"y").expect("writes");
            fs::create_dir_all(root.join("scratch")).expect("creates the tree");
            Self(root)
        }

        fn work(&self) -> PathBuf {
            self.0.join("work")
        }

        fn scratch(&self) -> PathBuf {
            self.0.join("scratch")
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// A minimal policy over `tree`'s write root.
    fn policy(tree: &Tree) -> Policy {
        Policy {
            fs: FsPolicy {
                entries: vec![FsEntry::write(tree.work())],
                protected: FsPolicy::default().protected,
            },
            shell: ShellPolicy {
                env: Vec::new(),
                workdir: tree.work(),
            },
            ..Policy::default()
        }
    }

    /// Whether `args` contains `needle` as an element.
    fn has(
        args: &[OsString],
        needle: &str,
    ) -> bool {
        args.iter().any(|a| a == needle)
    }

    /// The position of `needle`, or `None`.
    fn index_of(
        args: &[OsString],
        needle: &str,
    ) -> Option<usize> {
        args.iter().position(|a| a == needle)
    }

    #[test]
    fn a_write_root_is_bound_read_write() {
        let tree = Tree::new("bind");
        let args = render(&policy(&tree), &tree.scratch()).expect("renders");
        let at = index_of(&args, "--bind").expect("a bind");
        assert_eq!(args[at + 1], tree.work().into_os_string());
        assert_eq!(args[at + 2], tree.work().into_os_string());
    }

    #[test]
    fn the_read_grant_and_private_dev_come_first() {
        let tree = Tree::new("head");
        let args = render(&policy(&tree), &tree.scratch()).expect("renders");
        assert_eq!(
            &args[..5],
            [
                OsString::from("--ro-bind"),
                OsString::from("/"),
                OsString::from("/"),
                OsString::from("--dev"),
                OsString::from("/dev"),
            ]
        );
    }

    #[test]
    fn an_existing_protected_name_is_rebound_read_only() {
        let tree = Tree::new("protected");
        let args = render(&policy(&tree), &tree.scratch()).expect("renders");
        let protected = tree.work().join(".git");
        // There is a `--ro-bind <protected> <protected>` after the write bind.
        let rebound = args
            .windows(3)
            .any(|w| w[0] == "--ro-bind" && w[1] == protected && w[2] == protected);
        assert!(rebound, "the protected name should be re-bound read-only");
    }

    #[test]
    fn a_protected_name_that_does_not_exist_is_not_bound() {
        // The default name `.agents` is not in the tree, so nothing tries to
        // bind it; binding a missing path would make bwrap fail.
        let tree = Tree::new("absent-protected");
        let args = render(&policy(&tree), &tree.scratch()).expect("renders");
        let absent = tree.work().join(".agents");
        assert!(!args.iter().any(|a| a == &absent));
    }

    #[test]
    fn a_denied_directory_is_masked_and_made_read_only() {
        let tree = Tree::new("deny-dir");
        let mut policy = policy(&tree);
        policy
            .fs
            .entries
            .push(FsEntry::deny(tree.work().join("secret")));
        let args = render(&policy, &tree.scratch()).expect("renders");
        let secret = tree.work().join("secret");
        let masked = args.windows(4).any(|w| {
            w[0] == "--tmpfs" && w[1] == secret && w[2] == "--remount-ro" && w[3] == secret
        });
        assert!(masked, "a denied directory is a read-only tmpfs mask");
    }

    #[test]
    fn a_denied_file_is_masked_with_the_null_device() {
        let tree = Tree::new("deny-file");
        let mut policy = policy(&tree);
        policy
            .fs
            .entries
            .push(FsEntry::deny(tree.work().join("afile")));
        let args = render(&policy, &tree.scratch()).expect("renders");
        let file = tree.work().join("afile");
        let masked = args
            .windows(3)
            .any(|w| w[0] == "--ro-bind" && w[1] == "/dev/null" && w[2] == file);
        assert!(masked, "a denied file is the null device bound read-only");
    }

    #[test]
    fn a_deny_is_rendered_after_the_write_bind() {
        // Order matters: the mask must override the grant.
        let tree = Tree::new("order");
        let mut policy = policy(&tree);
        policy
            .fs
            .entries
            .push(FsEntry::deny(tree.work().join("secret")));
        let args = render(&policy, &tree.scratch()).expect("renders");
        let bind = index_of(&args, "--bind").expect("a bind");
        let mask = index_of(&args, "--tmpfs").expect("a mask");
        assert!(bind < mask, "the deny mask must follow the write bind");
    }

    #[test]
    fn the_environment_is_cleared_then_set() {
        let tree = Tree::new("env");
        let mut policy = policy(&tree);
        policy.shell.env = vec![crate::policy::EnvVar::new("LANG", "C")];
        let args = render(&policy, &tree.scratch()).expect("renders");
        let cleared = index_of(&args, "--clearenv").expect("clears the environment");
        let set = args
            .windows(3)
            .position(|w| w[0] == "--setenv" && w[1] == "LANG" && w[2] == "C")
            .expect("sets LANG");
        assert!(cleared < set, "the environment is cleared before it is set");
    }

    #[test]
    fn the_scratch_is_a_tmpfs_and_the_tmpdir() {
        let tree = Tree::new("scratch");
        let args = render(&policy(&tree), &tree.scratch()).expect("renders");
        assert!(has(&args, "--tmpfs"));
        let tmpdir = args
            .windows(3)
            .any(|w| w[0] == "--setenv" && w[1] == "TMPDIR" && w[2] == tree.scratch());
        assert!(tmpdir, "TMPDIR should name the scratch directory");
    }

    #[test]
    fn namespaces_are_unshared_and_the_process_dies_with_its_parent() {
        let tree = Tree::new("namespaces");
        let args = render(&policy(&tree), &tree.scratch()).expect("renders");
        assert!(has(&args, "--unshare-all"));
        assert!(has(&args, "--die-with-parent"));
    }

    #[test]
    fn the_workdir_is_the_chdir() {
        let tree = Tree::new("chdir");
        let args = render(&policy(&tree), &tree.scratch()).expect("renders");
        let at = index_of(&args, "--chdir").expect("a chdir");
        assert_eq!(args[at + 1], tree.work().into_os_string());
    }

    #[test]
    fn a_missing_write_root_is_reported() {
        let tree = Tree::new("missing-write");
        let mut policy = policy(&tree);
        policy.fs.entries = vec![FsEntry::write(tree.work().join("nope"))];
        policy.shell.workdir = tree.work().join("nope");
        assert!(matches!(
            render(&policy, &tree.scratch()),
            Err(RenderError::MissingWriteRoot { .. })
        ));
    }

    #[test]
    fn a_missing_deny_target_is_reported() {
        let tree = Tree::new("missing-deny");
        let mut policy = policy(&tree);
        policy
            .fs
            .entries
            .push(FsEntry::deny(tree.work().join("nope")));
        assert!(matches!(
            render(&policy, &tree.scratch()),
            Err(RenderError::MissingDenyTarget { .. })
        ));
    }

    #[test]
    fn a_scratch_inside_a_deny_is_reported() {
        // The scratch directory exists under the tree; denying it fires the
        // scratch check rather than the missing-target one.
        let tree = Tree::new("scratch-denied");
        let mut policy = policy(&tree);
        policy.fs.entries.push(FsEntry::deny(tree.scratch()));
        assert!(matches!(
            render(&policy, &tree.scratch()),
            Err(RenderError::ScratchDenied { .. })
        ));
    }

    /// A tree with a real Unix socket under `egress/`.
    fn socket_tree(tag: &str) -> (Tree, PathBuf) {
        let tree = Tree::new(tag);
        let dir = tree.0.join("egress");
        fs::create_dir_all(&dir).expect("creates the socket dir");
        let socket = dir.join("proxy.sock");
        std::os::unix::net::UnixListener::bind(&socket).expect("binds a socket");
        (tree, socket)
    }

    /// A policy over `tree`'s write root that grants egress over `socket`.
    fn policy_with_socket(
        tree: &Tree,
        socket: &Path,
    ) -> Policy {
        let mut policy = policy(tree);
        policy.network.proxy = Some(crate::policy::ProxyGrant {
            port: 8080,
            socket: Some(socket.to_path_buf()),
            egress: Vec::new(),
        });
        policy
    }

    #[test]
    fn an_egress_socket_is_dir_created_and_bound_read_write() {
        let (tree, socket) = socket_tree("egress-socket");
        let policy = policy_with_socket(&tree, &socket);
        let args = render(&policy, &tree.scratch()).expect("renders");

        let dir = index_of(&args, "--dir").expect("a --dir for the parent");
        assert_eq!(args[dir + 1], socket.parent().unwrap().as_os_str());
        // The socket is bound read-write, not read-only: `connect` needs write.
        let bound = args
            .windows(3)
            .any(|w| w[0] == "--bind" && w[1] == socket.as_os_str() && w[2] == socket.as_os_str());
        assert!(bound, "the socket should be bound read-write");
    }

    #[test]
    fn no_socket_is_mounted_without_an_egress_grant() {
        let tree = Tree::new("no-egress");
        let args = render(&policy(&tree), &tree.scratch()).expect("renders");
        assert!(!has(&args, "--dir"), "no socket dir is created");
    }

    #[test]
    fn a_missing_egress_socket_is_reported() {
        let tree = Tree::new("missing-socket");
        let missing = tree.0.join("egress/proxy.sock");
        let policy = policy_with_socket(&tree, &missing);
        assert!(matches!(
            render(&policy, &tree.scratch()),
            Err(RenderError::MissingEgressSocket { .. })
        ));
    }

    #[test]
    fn an_egress_socket_inside_a_deny_is_reported() {
        let (tree, socket) = socket_tree("socket-denied");
        let policy = policy_with_socket(&tree, &socket);
        let mut policy = policy;
        // Deny the socket's directory, which covers the socket.
        policy
            .fs
            .entries
            .push(FsEntry::deny(socket.parent().unwrap()));
        assert!(matches!(
            render(&policy, &tree.scratch()),
            Err(RenderError::SocketDenied { .. })
        ));
    }
}
