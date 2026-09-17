//! Layer-1 confinement for Linux.
//!
//! Commands spawn as real processes under **bubblewrap** (preferred) or, when
//! bubblewrap is unavailable, under a **Landlock** ruleset plus a seccomp filter
//! applied by the [`helper`](crate::helper) binary.
//!
//! **Bubblewrap** renders the policy into `bwrap` arguments that build a mount
//! namespace and drop the network: the host root is bound read-only (reads are
//! broad, as on macOS), write entries are bound read-write, an existing
//! protected name is re-bound read-only, and `deny` entries are masked after
//! the binds so a nested denial holds.
//!
//! **Landlock** is an allowlist, so the policy is rendered as the set of paths
//! the command may reach, and the helper applies it before `exec`. Landlock
//! cannot subtract, so a `deny` nested in a write root is not enforced there.
//!
//! Stated gaps versus the macOS backend: a masked `deny` hides a directory
//! entirely rather than carving it out read-only; a fresh protected name can be
//! created; and `AF_UNIX` sockets are not path-scoped, because neither
//! `--unshare-all` nor Landlock isolates them by path. See `docs/sandbox.md`.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::error::SandboxError;
use crate::executor::{ExecResult, SpawnError};
use crate::helper::{PathAccess, PathRule, Spec};
use crate::policy::{Access, FsPolicy, Policy};
use crate::process;

/// The shell the command runs under.
const BASH: &str = "/bin/bash";

/// The bubblewrap binary looked up on `PATH`.
const BWRAP: &str = "bwrap";

/// The Landlock helper binary looked up on `PATH`, or set explicitly with
/// `AGENTD_SANDBOX_HELPER`.
const HELPER: &str = "agentd-sandbox-helper";

/// The environment variable that overrides where the Landlock helper is found.
const HELPER_ENV: &str = "AGENTD_SANDBOX_HELPER";

/// The host roots a confined shell and its binaries need, bound read-only and
/// executable on Linux. `/bin` and `/lib` may be symlinks into `/usr`, which
/// Landlock resolves.
const SYSTEM_ROOTS: &[&str] = &[
    "/bin", "/sbin", "/lib", "/lib32", "/lib64", "/libx32", "/usr", "/etc",
];

/// Character devices a command commonly needs, granted read-write.
const DEVICES: &[&str] = &["/dev/null", "/dev/zero", "/dev/urandom", "/dev/random"];

/// The Linux layer-1 executor: renders the policy into a bubblewrap command or
/// a Landlock spec at construction and spawns commands under it.
///
/// The policy is bound at construction, so a running executor cannot widen its
/// own permissions.
pub struct ConfinedProcessExecutor {
    backend: Backend,
    scratch: PathBuf,
    workdir: PathBuf,
    /// Read-write roots, for the bubblewrap backend.
    writes: Vec<PathBuf>,
    /// Read-only carve-outs inside a write root, for the bubblewrap backend.
    protected: Vec<PathBuf>,
    /// Masked denials, for the bubblewrap backend.
    denies: Vec<Deny>,
    env: Vec<(String, String)>,
    path_env: String,
    timeout: Duration,
    max_output_bytes: u64,
}

/// How the policy is enforced.
#[derive(Debug)]
enum Backend {
    /// Spawn under `bwrap` with the rendered mounts.
    Bubblewrap(PathBuf),
    /// Spawn the Landlock helper with a serialized spec.
    Landlock { helper: PathBuf, spec_path: PathBuf },
}

/// A resolved `deny` entry and how to mask it in the sandbox.
struct Deny {
    path: PathBuf,
    kind: DenyKind,
}

/// How a masked `deny` path is removed from the sandbox.
enum DenyKind {
    /// A directory is replaced with an empty `tmpfs`.
    Directory,
    /// A file is replaced by `/dev/null`.
    File,
}

/// The policy rendered for the bubblewrap backend.
struct Resolved {
    /// Read-write roots.
    writes: Vec<PathBuf>,
    /// Read entries, canonicalized and existing, for the Landlock spec.
    reads: Vec<PathBuf>,
    /// Paths re-bound read-only inside a write root.
    protected: Vec<PathBuf>,
    /// Paths masked out of the sandbox.
    denies: Vec<Deny>,
}

impl Drop for ConfinedProcessExecutor {
    fn drop(&mut self) {
        // Best-effort cleanup; a leaked scratch directory holds nothing the
        // host cares about.
        let _ = fs::remove_dir_all(&self.scratch);
    }
}

impl std::fmt::Debug for ConfinedProcessExecutor {
    fn fmt(
        &self,
        formatter: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        formatter
            .debug_struct("ConfinedProcessExecutor")
            .field("backend", &self.backend)
            .field("workdir", &self.workdir)
            .field("scratch", &self.scratch)
            .finish_non_exhaustive()
    }
}

impl ConfinedProcessExecutor {
    /// Builds the executor from the policy, preferring bubblewrap and falling
    /// back to Landlock.
    ///
    /// # Errors
    ///
    /// Fails closed with [`SandboxError::UnsupportedPlatform`] when neither a
    /// trusted `bwrap` nor the Landlock helper is available,
    /// [`SandboxError::Policy`] when the policy fails validation, and
    /// [`SandboxError::InvalidPolicy`] for an unusable workdir or path entry.
    pub fn new(policy: &Policy) -> Result<Self, SandboxError> {
        Self::build(policy, None)
    }

    /// Builds the executor around a specific Landlock helper, bypassing the
    /// bubblewrap preference. Tests use this to exercise the fallback.
    ///
    /// # Errors
    ///
    /// As [`new`](Self::new).
    #[cfg(test)]
    fn with_landlock(
        policy: &Policy,
        helper: PathBuf,
    ) -> Result<Self, SandboxError> {
        Self::build(policy, Some(ForcedBackend::Landlock(helper)))
    }

    /// Builds the executor around a specific bubblewrap path, bypassing the
    /// availability probe. Tests use this to render without `bwrap` installed.
    ///
    /// # Errors
    ///
    /// As [`new`](Self::new).
    #[cfg(test)]
    fn with_bwrap(
        policy: &Policy,
        bwrap: PathBuf,
    ) -> Result<Self, SandboxError> {
        Self::build(policy, Some(ForcedBackend::Bubblewrap(bwrap)))
    }

    fn build(
        policy: &Policy,
        forced: Option<ForcedBackend>,
    ) -> Result<Self, SandboxError> {
        policy.validate()?;
        let workdir = process::validate_workdir(&policy.shell.workdir, &policy.fs)?;
        let path_dirs = process::system_path_dirs();
        let path_env = process::join_path(&path_dirs)?;
        let scratch = process::create_scratch()?;
        let resolved =
            resolve_entries(&policy.fs, &workdir, &scratch, &path_dirs).inspect_err(|_| {
                let _ = fs::remove_dir_all(&scratch);
            })?;
        let backend = select_backend(policy, forced, &resolved, &scratch).inspect_err(|_| {
            let _ = fs::remove_dir_all(&scratch);
        })?;
        let env = policy
            .shell
            .env
            .iter()
            .map(|variable| (variable.name.clone(), variable.value.clone()))
            .collect();
        Ok(Self {
            backend,
            scratch,
            workdir,
            writes: resolved.writes,
            protected: resolved.protected,
            denies: resolved.denies,
            env,
            path_env,
            timeout: policy.limits.timeout,
            max_output_bytes: policy.limits.max_output_bytes,
        })
    }

    /// Builds the confined command for `command`, with the policy's mounts, the
    /// allowlisted environment, and the scratch `HOME`/`TMPDIR`.
    fn std_command(
        &self,
        command: &str,
    ) -> std::process::Command {
        let mut std_command = match &self.backend {
            Backend::Bubblewrap(bwrap) => self.bubblewrap_command(bwrap, command),
            Backend::Landlock { helper, spec_path } => {
                let mut std_command = std::process::Command::new(helper);
                std_command
                    .arg(spec_path)
                    .arg(process::BASH)
                    .arg("-c")
                    .arg(command);
                std_command
            },
        };
        std_command.env_clear();
        for (name, value) in &self.env {
            std_command.env(name, value);
        }
        std_command.env("PATH", &self.path_env);
        std_command.env("HOME", &self.scratch);
        std_command.env("TMPDIR", &self.scratch);
        std_command.current_dir(&self.workdir);
        // The child leads its own process group so a timeout, output-cap, or
        // session kill takes the whole tree down, not just the shell.
        std_command.process_group(0);
        std_command
    }

    /// The `bwrap` command with the policy's mounts.
    fn bubblewrap_command(
        &self,
        bwrap: &Path,
        command: &str,
    ) -> std::process::Command {
        let mut std_command = std::process::Command::new(bwrap);
        std_command
            .arg("--die-with-parent")
            .arg("--unshare-all")
            // The whole host root is readable (reads are broad, as on macOS);
            // writes are granted below by overriding subtrees.
            .arg("--ro-bind")
            .arg("/")
            .arg("/")
            .arg("--dev")
            .arg("/dev")
            .arg("--proc")
            .arg("/proc")
            .arg("--tmpfs")
            .arg("/tmp")
            .arg("--bind")
            .arg(&self.scratch)
            .arg(&self.scratch);
        for write in &self.writes {
            std_command.arg("--bind").arg(write).arg(write);
        }
        // Protected metadata inside a write root is re-bound read-only, so a
        // command cannot rewrite the repo history or its own instructions.
        // `--ro-bind-try` skips a name that does not exist yet, so creation of
        // a fresh `.git` is not prevented — a stated gap.
        for path in &self.protected {
            std_command.arg("--ro-bind-try").arg(path).arg(path);
        }
        // Mask denials after the grants: a later mount overrides an earlier
        // one, so a `deny` nested in a write root wins.
        for deny in &self.denies {
            match deny.kind {
                DenyKind::Directory => {
                    std_command.arg("--tmpfs").arg(&deny.path);
                },
                DenyKind::File => {
                    std_command
                        .arg("--ro-bind")
                        .arg("/dev/null")
                        .arg(&deny.path);
                },
            }
        }
        std_command
            .arg("--chdir")
            .arg(&self.workdir)
            .arg("--")
            .arg(BASH)
            .arg("-c")
            .arg(command);
        std_command
    }

    async fn run(
        &self,
        command: &str,
    ) -> ExecResult {
        process::run(
            self.std_command(command),
            self.timeout,
            self.max_output_bytes,
        )
        .await
    }
}

#[async_trait::async_trait]
impl crate::executor::Executor for ConfinedProcessExecutor {
    async fn exec(
        &self,
        command: &str,
    ) -> ExecResult {
        Box::pin(self.run(command)).await
    }

    async fn spawn(
        &self,
        command: &str,
    ) -> Result<tokio::process::Child, SpawnError> {
        process::spawn(self.std_command(command))
    }
}

/// A backend chosen by a test instead of by availability.
///
/// Only the test-only constructors build these variants, so they look dead in a
/// normal build.
#[cfg_attr(not(test), allow(dead_code))]
enum ForcedBackend {
    Bubblewrap(PathBuf),
    Landlock(PathBuf),
}

/// Chooses the backend: a forced one, else `bwrap`, else the Landlock helper.
fn select_backend(
    policy: &Policy,
    forced: Option<ForcedBackend>,
    resolved: &Resolved,
    scratch: &Path,
) -> Result<Backend, SandboxError> {
    match forced {
        Some(ForcedBackend::Bubblewrap(bwrap)) => Ok(Backend::Bubblewrap(bwrap)),
        Some(ForcedBackend::Landlock(helper)) => landlock_backend(&helper, resolved, scratch),
        None => {
            if let Some(bwrap) = find_on_path(BWRAP, Some(&policy.fs)) {
                return Ok(Backend::Bubblewrap(bwrap));
            }
            let helper = find_helper(&policy.fs)?;
            landlock_backend(&helper, resolved, scratch)
        },
    }
}

/// Writes the Landlock spec and returns the helper backend.
fn landlock_backend(
    helper: &Path,
    resolved: &Resolved,
    scratch: &Path,
) -> Result<Backend, SandboxError> {
    ensure_denies_enforceable(resolved, scratch)?;
    let spec = build_spec(resolved, scratch);
    let spec_path = scratch.join("spec.json");
    fs::write(
        &spec_path,
        serde_json::to_vec(&spec).map_err(|error| {
            SandboxError::InvalidPolicy(format!("cannot serialize the spec: {error}"))
        })?,
    )?;
    Ok(Backend::Landlock {
        helper: helper.to_path_buf(),
        spec_path,
    })
}

/// Rejects a `deny` the Landlock allowlist cannot express.
///
/// Landlock rules can only grant; they cannot subtract. A `deny` nested inside
/// an allowed tree would therefore be silently unenforced, which violates
/// deny-by-default, so the fallback fails closed instead: the operator must
/// use a host with bubblewrap or restructure the policy.
fn ensure_denies_enforceable(
    resolved: &Resolved,
    scratch: &Path,
) -> Result<(), SandboxError> {
    let granted = granted_paths(resolved, scratch);
    for deny in &resolved.denies {
        if let Some(root) = granted.iter().find(|root| deny.path.starts_with(root)) {
            return Err(SandboxError::InvalidPolicy(format!(
                "deny path {:?} is inside allowed {:?}, which the Landlock fallback cannot narrow",
                deny.path.display().to_string(),
                root.display().to_string()
            )));
        }
    }
    Ok(())
}

/// The canonical paths the Landlock allowlist grants, for deny comparison.
fn granted_paths(
    resolved: &Resolved,
    scratch: &Path,
) -> Vec<PathBuf> {
    let mut granted: Vec<PathBuf> = SYSTEM_ROOTS
        .iter()
        .copied()
        .chain(DEVICES.iter().copied())
        .chain(["/proc"])
        .filter_map(|path| PathBuf::from(path).canonicalize().ok())
        .collect();
    granted.extend(resolved.reads.iter().cloned());
    granted.extend(resolved.writes.iter().cloned());
    granted.extend(scratch.canonicalize());
    granted.sort();
    granted.dedup();
    granted
}

/// Renders the resolved policy into a Landlock spec.
///
/// System roots and devices a shell needs are granted; read entries are granted
/// read-only; write roots and the scratch directory are granted read-write;
/// `deny` entries are simply omitted, because Landlock cannot subtract.
fn build_spec(
    resolved: &Resolved,
    scratch: &Path,
) -> Spec {
    let mut paths: Vec<PathRule> = Vec::new();
    for root in SYSTEM_ROOTS {
        push_existing(&mut paths, PathBuf::from(root), PathAccess::ReadExecute);
    }
    for device in DEVICES {
        push_existing(&mut paths, PathBuf::from(device), PathAccess::Write);
    }
    push_existing(&mut paths, PathBuf::from("/proc"), PathAccess::Read);
    paths.extend(resolved.reads.iter().cloned().map(|path| PathRule {
        path,
        access: PathAccess::Read,
    }));
    paths.extend(resolved.writes.iter().cloned().map(|path| PathRule {
        path,
        access: PathAccess::Write,
    }));
    paths.push(PathRule {
        path: scratch.to_path_buf(),
        access: PathAccess::Write,
    });
    paths.sort_by(|left, right| left.path.cmp(&right.path));
    paths.dedup_by(|left, right| left.path == right.path);
    Spec { paths }
}

/// Adds a rule for `path` when it exists.
fn push_existing(
    paths: &mut Vec<PathRule>,
    path: PathBuf,
    access: PathAccess,
) {
    if path.exists() {
        paths.push(PathRule { path, access });
    }
}

/// Finds a system binary on `PATH`, excluding any inside a policy write root.
///
/// A repository could otherwise supply the very binary that builds the
/// boundary; the daemon's write roots are the workspaces it edits, so a binary
/// under one is not trusted.
fn find_on_path(
    name: &str,
    fs_policy: Option<&FsPolicy>,
) -> Option<PathBuf> {
    let workspace_roots: Vec<PathBuf> = fs_policy
        .map(|policy| {
            policy
                .entries
                .iter()
                .filter(|entry| entry.access == Access::Write)
                .filter_map(|entry| entry.path.canonicalize().ok())
                .collect()
        })
        .unwrap_or_default();
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| {
        let candidate = dir.join(name);
        if !is_executable(&candidate) {
            return None;
        }
        let canonical = candidate.canonicalize().ok()?;
        if workspace_roots
            .iter()
            .any(|root| canonical.starts_with(root))
        {
            return None;
        }
        Some(candidate)
    })
}

/// Finds the Landlock helper: the `AGENTD_SANDBOX_HELPER` override, else on
/// `PATH`.
fn find_helper(fs_policy: &FsPolicy) -> Result<PathBuf, SandboxError> {
    if let Some(configured) = std::env::var_os(HELPER_ENV) {
        let candidate = PathBuf::from(configured);
        if is_executable(&candidate) && find_on_path_is_trusted(&candidate, fs_policy) {
            return Ok(candidate);
        }
        return Err(SandboxError::UnsupportedPlatform(
            "the configured AGENTD_SANDBOX_HELPER is not an executable outside the workspace",
        ));
    }
    find_on_path(HELPER, Some(fs_policy)).ok_or(SandboxError::UnsupportedPlatform(
        "no confinement backend: bubblewrap and the `agentd-sandbox-helper` are both unavailable",
    ))
}

/// Whether `candidate` is outside every write root.
fn find_on_path_is_trusted(
    candidate: &Path,
    fs_policy: &FsPolicy,
) -> bool {
    let Ok(canonical) = candidate.canonicalize() else {
        return false;
    };
    !fs_policy
        .entries
        .iter()
        .filter(|entry| entry.access == Access::Write)
        .filter_map(|entry| entry.path.canonicalize().ok())
        .any(|root| canonical.starts_with(root))
}

/// Whether `path` is a regular executable file.
fn is_executable(path: &Path) -> bool {
    fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

/// Resolves the write, read, protected, and deny entries into canonical paths.
///
/// A `write` or `deny` entry must resolve: an unresolvable path cannot be
/// expressed and dropping it would weaken the sandbox. A `deny` that covers a
/// path every command needs — the workdir, an executable directory, the scratch
/// directory, or a write root — is rejected, because masking it would break
/// every command rather than narrow one path. A name in `protected` that exists
/// inside a write root is carved out read-only. A `read` entry that does not
/// resolve is dropped: it grants nothing.
fn resolve_entries(
    fs_policy: &FsPolicy,
    workdir: &Path,
    scratch: &Path,
    path_dirs: &[PathBuf],
) -> Result<Resolved, SandboxError> {
    let mut writes: Vec<PathBuf> = Vec::new();
    let mut reads: Vec<PathBuf> = Vec::new();
    for entry in &fs_policy.entries {
        match entry.access {
            Access::Write => {
                let canonical = entry.path.canonicalize().map_err(|error| {
                    SandboxError::InvalidPolicy(format!(
                        "write entry {:?} is not accessible: {error}",
                        entry.path.display().to_string()
                    ))
                })?;
                writes.push(canonical);
            },
            Access::Read => {
                if let Ok(canonical) = entry.path.canonicalize() {
                    reads.push(canonical);
                }
            },
            Access::Deny => {},
        }
    }
    writes.sort();
    writes.dedup();
    reads.sort();
    reads.dedup();

    let mut needed: Vec<(&str, PathBuf)> = Vec::new();
    if let Ok(canonical) = workdir.canonicalize() {
        needed.push(("the workdir", canonical));
    }
    if let Ok(canonical) = scratch.canonicalize() {
        needed.push(("the scratch directory", canonical));
    }
    needed.extend(
        path_dirs
            .iter()
            .filter_map(|dir| dir.canonicalize().ok())
            .map(|canonical| ("an executable directory", canonical)),
    );
    needed.extend(writes.iter().cloned().map(|root| ("a write root", root)));

    let mut denies: Vec<Deny> = Vec::new();
    for entry in &fs_policy.entries {
        if entry.access != Access::Deny {
            continue;
        }
        if !entry.path.is_absolute() {
            return Err(SandboxError::InvalidPolicy(format!(
                "deny path {:?} must be an absolute host path",
                entry.path.display().to_string()
            )));
        }
        let canonical = entry.path.canonicalize().map_err(|error| {
            SandboxError::InvalidPolicy(format!(
                "deny path {:?} is not accessible: {error}",
                entry.path.display().to_string()
            ))
        })?;
        for (role, needed_path) in &needed {
            // A denial that *covers* a path a command needs (an ancestor of the
            // needed path) breaks every command, so it is rejected. A denial
            // nested inside a write root only masks that subtree — e.g.
            // `deny /repo/.env` inside the `/repo` write root — and is allowed.
            if needed_path.starts_with(&canonical) {
                return Err(SandboxError::InvalidPolicy(format!(
                    "deny path {:?} covers {role} {:?}, which every command needs",
                    entry.path.display().to_string(),
                    needed_path.display().to_string()
                )));
            }
        }
        let metadata = fs::metadata(&canonical)?;
        let kind = if metadata.is_dir() {
            DenyKind::Directory
        } else {
            DenyKind::File
        };
        denies.push(Deny {
            path: canonical,
            kind,
        });
    }
    denies.sort_by(|left, right| left.path.cmp(&right.path));
    denies.dedup_by(|left, right| left.path == right.path);

    let mut protected: Vec<PathBuf> = writes
        .iter()
        .flat_map(|root| fs_policy.protected.iter().map(move |name| root.join(name)))
        .filter(|candidate| candidate.exists())
        .collect();
    protected.sort();
    protected.dedup();
    Ok(Resolved {
        writes,
        reads,
        protected,
        denies,
    })
}

#[cfg(test)]
mod tests {
    use super::{BWRAP, Backend, ConfinedProcessExecutor, find_on_path};
    use crate::executor::Executor;
    use crate::helper::{PathAccess, Spec};
    use crate::policy::{Access, EnvVar, FsEntry, FsPolicy, Policy, ShellPolicy};
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};
    use tempfile::TempDir;

    /// An executor whose workdir is `workdir` and whose only write entry is it.
    fn workdir_policy(workdir: &Path) -> Policy {
        Policy {
            fs: FsPolicy {
                entries: vec![FsEntry {
                    path: workdir.to_path_buf(),
                    access: Access::Write,
                }],
                ..FsPolicy::default()
            },
            shell: ShellPolicy {
                workdir: workdir.to_path_buf(),
                ..ShellPolicy::default()
            },
            ..Policy::default()
        }
    }

    /// The helper binary built alongside the tests.
    fn helper_path() -> PathBuf {
        if let Some(path) = option_env!("CARGO_BIN_EXE_agentd-sandbox-helper") {
            return PathBuf::from(path);
        }
        let mut dir = std::env::current_exe().expect("the test binary has a path");
        dir.pop();
        dir.push("agentd-sandbox-helper");
        dir
    }

    /// Whether bubblewrap can actually build a namespace here. The Nix build
    /// sandbox (and an unprivileged host with user namespaces disabled) cannot,
    /// so the spawn tests skip rather than fail there.
    fn spawn_tests_supported() -> bool {
        if std::env::var_os("NIX_BUILD_TOP").is_some() {
            return false;
        }
        let Some(bwrap) = find_on_path(BWRAP, None) else {
            return false;
        };
        std::process::Command::new(bwrap)
            .args(["--ro-bind", "/", "/", "--", "/bin/true"])
            .status()
            .is_ok_and(|status| status.success())
    }

    /// A Landlock executor, or `None` when the helper cannot confine here.
    fn landlock_executor(policy: &Policy) -> Option<ConfinedProcessExecutor> {
        if !helper_path().exists() {
            return None;
        }
        let executor = ConfinedProcessExecutor::with_landlock(policy, helper_path()).ok()?;
        // Probe the kernel: a Landlock-unsupported host fails closed before the
        // shell runs, and the test skips rather than report a false failure.
        if executor.blocking_exec("true").exit_code != 0 {
            return None;
        }
        Some(executor)
    }

    fn executor(policy: &Policy) -> ConfinedProcessExecutor {
        ConfinedProcessExecutor::new(policy).expect("a valid policy builds an executor")
    }

    fn args(command: &std::process::Command) -> Vec<String> {
        command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    fn window(
        haystack: &[String],
        needle: &[&str],
    ) -> bool {
        haystack
            .windows(needle.len())
            .any(|window| window.iter().zip(needle).all(|(a, b)| a == b))
    }

    #[test]
    fn bwrap_renders_the_policy() {
        let dir = TempDir::new().expect("tempdir");
        let host = dir.path().canonicalize().expect("canonical tempdir");
        let write_root = host.join("rw");
        let deny_dir = host.join("secret");
        std::fs::create_dir(&write_root).expect("rw dir");
        std::fs::create_dir(&deny_dir).expect("secret dir");
        std::fs::write(deny_dir.join("token"), "s3cr3t").expect("secret file");

        let policy = Policy {
            fs: FsPolicy {
                entries: vec![
                    FsEntry {
                        path: write_root.clone(),
                        access: Access::Write,
                    },
                    FsEntry {
                        path: deny_dir.clone(),
                        access: Access::Deny,
                    },
                ],
                ..FsPolicy::default()
            },
            shell: ShellPolicy {
                workdir: write_root.clone(),
                ..ShellPolicy::default()
            },
            ..Policy::default()
        };
        let executor =
            ConfinedProcessExecutor::with_bwrap(&policy, PathBuf::from("/usr/bin/bwrap"))
                .expect("the policy renders");

        let args = args(&executor.std_command("true"));
        // Reads are broad: the host root is bound read-only.
        assert!(window(&args, &["--ro-bind", "/", "/"]));
        // The write root is bound read-write, and the deny is masked after it.
        assert!(window(
            &args,
            &[
                "--bind",
                &write_root.display().to_string(),
                &write_root.display().to_string()
            ]
        ));
        assert!(window(&args, &["--tmpfs", &deny_dir.display().to_string()]));
        assert!(args.iter().any(|arg| arg == "--unshare-all"));
        assert!(args.iter().any(|arg| arg == "--die-with-parent"));
        assert!(args.iter().any(|arg| arg == "--chdir"));
    }

    #[test]
    fn protected_metadata_is_carved_out_read_only() {
        let dir = TempDir::new().expect("tempdir");
        let host = dir.path().canonicalize().expect("canonical tempdir");
        std::fs::create_dir_all(host.join(".git/hooks")).expect("git dir");
        let policy = workdir_policy(&host);
        let executor =
            ConfinedProcessExecutor::with_bwrap(&policy, PathBuf::from("/usr/bin/bwrap"))
                .expect("the policy renders");

        let args = args(&executor.std_command("true"));
        let git = host.join(".git").display().to_string();
        assert!(
            window(&args, &["--ro-bind-try", &git, &git]),
            "an existing .git must be re-bound read-only: {args:?}"
        );
    }

    #[test]
    fn deny_covering_a_needed_path_is_rejected() {
        let dir = TempDir::new().expect("tempdir");
        let host = dir.path().canonicalize().expect("canonical tempdir");
        // A denial of the workdir (or any ancestor) would mask the whole
        // workspace, so construction fails closed.
        let policy = Policy {
            fs: FsPolicy {
                entries: vec![
                    FsEntry {
                        path: host.clone(),
                        access: Access::Write,
                    },
                    FsEntry {
                        path: host.clone(),
                        access: Access::Deny,
                    },
                ],
                ..FsPolicy::default()
            },
            shell: ShellPolicy {
                workdir: host,
                ..ShellPolicy::default()
            },
            ..Policy::default()
        };

        assert!(matches!(
            ConfinedProcessExecutor::new(&policy),
            Err(crate::error::SandboxError::InvalidPolicy(_))
        ));
    }

    #[test]
    fn landlock_rejects_a_nested_deny_it_cannot_enforce() {
        let dir = TempDir::new().expect("tempdir");
        let host = dir.path().canonicalize().expect("canonical tempdir");
        std::fs::write(host.join(".env"), "secret").expect("env file");
        // A deny nested in the write root is enforced by bubblewrap (masked)
        // but cannot be expressed by Landlock, so the fallback fails closed
        // rather than silently leaving `.env` readable and writable.
        let policy = Policy {
            fs: FsPolicy {
                entries: vec![
                    FsEntry {
                        path: host.clone(),
                        access: Access::Write,
                    },
                    FsEntry {
                        path: host.join(".env"),
                        access: Access::Deny,
                    },
                ],
                ..FsPolicy::default()
            },
            shell: ShellPolicy {
                workdir: host,
                ..ShellPolicy::default()
            },
            ..Policy::default()
        };

        assert!(matches!(
            ConfinedProcessExecutor::with_landlock(&policy, PathBuf::from("helper")),
            Err(crate::error::SandboxError::InvalidPolicy(_))
        ));
    }

    #[test]
    fn landlock_spec_lists_system_and_write_roots() {
        let dir = TempDir::new().expect("tempdir");
        let host = dir.path().canonicalize().expect("canonical tempdir");
        let policy = workdir_policy(&host);
        let executor =
            ConfinedProcessExecutor::with_landlock(&policy, helper_path()).expect("landlock spec");

        let Backend::Landlock { spec_path, .. } = &executor.backend else {
            panic!("the executor must use the Landlock backend");
        };
        let spec: Spec =
            serde_json::from_slice(&std::fs::read(spec_path).expect("the spec is written"))
                .expect("the spec parses");

        assert!(
            spec.paths
                .iter()
                .any(|rule| rule.path == host && matches!(rule.access, PathAccess::Write)),
            "the write root must be granted: {spec:?}"
        );
        // The exact system roots depend on the host layout (e.g. `/usr` may be
        // absent on a minimal image), so assert the read-execute class is
        // present rather than a specific directory.
        assert!(
            spec.paths
                .iter()
                .any(|rule| matches!(rule.access, PathAccess::ReadExecute)),
            "the system roots must be granted read-execute: {spec:?}"
        );
    }

    #[test]
    fn writes_reach_write_entries_but_not_denied_paths() {
        if !spawn_tests_supported() {
            return;
        }
        let dir = TempDir::new().expect("tempdir");
        let host = dir.path().canonicalize().expect("canonical tempdir");
        let write_root = host.join("rw");
        let deny_dir = host.join("secret");
        std::fs::create_dir(&write_root).expect("rw dir");
        std::fs::create_dir(&deny_dir).expect("secret dir");
        std::fs::write(deny_dir.join("token"), "s3cr3t").expect("secret file");

        let policy = Policy {
            fs: FsPolicy {
                entries: vec![
                    FsEntry {
                        path: write_root.clone(),
                        access: Access::Write,
                    },
                    FsEntry {
                        path: deny_dir.clone(),
                        access: Access::Deny,
                    },
                ],
                ..FsPolicy::default()
            },
            shell: ShellPolicy {
                workdir: write_root.clone(),
                ..ShellPolicy::default()
            },
            ..Policy::default()
        };
        let executor = executor(&policy);

        let written = executor.blocking_exec("echo data > out.txt");
        assert_eq!(written.exit_code, 0, "stderr: {}", written.stderr);
        assert!(write_root.join("out.txt").exists());

        let outside = executor.blocking_exec("touch /etc/agentd-blocked");
        assert_ne!(outside.exit_code, 0, "stderr: {}", outside.stderr);

        let denied = executor.blocking_exec(&format!("cat {}/token", deny_dir.display()));
        assert_ne!(denied.exit_code, 0, "stdout: {}", denied.stdout);
        assert!(!denied.stdout.contains("s3cr3t"));
    }

    #[test]
    fn exec_echoes_and_clears_the_environment() {
        if !spawn_tests_supported() {
            return;
        }
        let dir = TempDir::new().expect("tempdir");
        let host = dir.path().canonicalize().expect("canonical tempdir");
        let mut policy = workdir_policy(&host);
        policy.shell.env = vec![EnvVar {
            name: String::from("SANDBOX_MARKER"),
            value: String::from("present"),
        }];
        let executor = executor(&policy);

        let result = executor.blocking_exec("echo $SANDBOX_MARKER");
        assert_eq!(result.exit_code, 0, "stderr: {}", result.stderr);
        assert_eq!(result.stdout, "present\n");
    }

    #[tokio::test]
    async fn a_spawned_session_streams_io() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        if !spawn_tests_supported() {
            return;
        }
        let dir = TempDir::new().expect("tempdir");
        let host = dir.path().canonicalize().expect("canonical tempdir");
        let executor = executor(&workdir_policy(&host));

        let mut child = executor.spawn("cat").await.expect("session should spawn");
        let mut stdin = child.stdin.take().expect("stdin");
        let mut stdout = child.stdout.take().expect("stdout");
        stdin.write_all(b"hello\n").await.expect("write stdin");
        drop(stdin);
        let mut line = String::new();
        stdout.read_to_string(&mut line).await.expect("read stdout");

        assert_eq!(line, "hello\n");
        assert!(child.wait().await.expect("wait").success());
    }

    #[test]
    fn timeout_kills_the_process_group() {
        if !spawn_tests_supported() {
            return;
        }
        let dir = TempDir::new().expect("tempdir");
        let host = dir.path().canonicalize().expect("canonical tempdir");
        let mut policy = workdir_policy(&host);
        policy.limits.timeout = Duration::from_secs(1);
        let executor = executor(&policy);

        let started = Instant::now();
        let result = executor.blocking_exec("sleep 30 & wait");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "must not wait 30s"
        );
        assert_eq!(result.exit_code, 124);
        assert!(result.stderr.contains("timeout"));
    }

    #[test]
    fn landlock_confines_the_filesystem_and_seccomp() {
        let dir = TempDir::new().expect("tempdir");
        let host = dir.path().canonicalize().expect("canonical tempdir");
        let write_root = host.join("rw");
        let deny_dir = host.join("secret");
        std::fs::create_dir(&write_root).expect("rw dir");
        std::fs::create_dir(&deny_dir).expect("secret dir");
        std::fs::write(deny_dir.join("token"), "s3cr3t").expect("secret file");

        let policy = Policy {
            fs: FsPolicy {
                entries: vec![
                    FsEntry {
                        path: write_root.clone(),
                        access: Access::Write,
                    },
                    FsEntry {
                        path: deny_dir.clone(),
                        access: Access::Deny,
                    },
                ],
                ..FsPolicy::default()
            },
            shell: ShellPolicy {
                workdir: write_root.clone(),
                ..ShellPolicy::default()
            },
            ..Policy::default()
        };
        let Some(executor) = landlock_executor(&policy) else {
            return;
        };

        // The write root is reachable, and an unrelated host path is not.
        let written = executor.blocking_exec("echo data > out.txt");
        assert_eq!(written.exit_code, 0, "stderr: {}", written.stderr);
        assert!(write_root.join("out.txt").exists());

        let outside = executor.blocking_exec("touch /etc/agentd-blocked");
        assert_ne!(outside.exit_code, 0, "stderr: {}", outside.stderr);

        // A denied path is simply not granted, so it is unreadable.
        let denied = executor.blocking_exec(&format!("cat {}/token", deny_dir.display()));
        assert_ne!(denied.exit_code, 0, "stdout: {}", denied.stdout);
        assert!(!denied.stdout.contains("s3cr3t"));

        // Seccomp is installed even though it blocks no ordinary command: the
        // status file reports `Seccomp: 2` once a filter is active.
        let seccomp = executor.blocking_exec("grep -q 'Seccomp:[[:space:]]*2' /proc/self/status");
        assert_eq!(seccomp.exit_code, 0, "stderr: {}", seccomp.stderr);
        let allowed = executor.blocking_exec("echo ok");
        assert_eq!(allowed.exit_code, 0, "stderr: {}", allowed.stderr);
        assert_eq!(allowed.stdout, "ok\n");
    }

    #[test]
    fn the_seccomp_blocklist_covers_dangerous_calls() {
        use crate::helper::BLOCKED_SYSCALLS;

        for syscall in [
            libc::SYS_ptrace,
            libc::SYS_io_uring_setup,
            libc::SYS_bpf,
            libc::SYS_userfaultfd,
            libc::SYS_keyctl,
        ] {
            assert!(
                BLOCKED_SYSCALLS.contains(&syscall),
                "syscall {syscall} must be refused"
            );
        }
    }

    /// Runs the executor on the current thread's runtime: the spawn tests are
    /// synchronous end to end.
    trait BlockingExec {
        fn blocking_exec(
            &self,
            command: &str,
        ) -> crate::executor::ExecResult;
    }

    impl BlockingExec for ConfinedProcessExecutor {
        fn blocking_exec(
            &self,
            command: &str,
        ) -> crate::executor::ExecResult {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime")
                .block_on(self.exec(command))
        }
    }
}
