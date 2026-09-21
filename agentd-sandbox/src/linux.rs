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
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::error::SandboxError;
use crate::executor::{ExecResult, SpawnError};
use crate::helper::{PathAccess, PathRule, Spec};
use crate::policy::{Access, FsPolicy, Policy};
use crate::process;

/// The bubblewrap binary looked up on `PATH`.
const BWRAP: &str = "bwrap";

/// The Landlock helper binary looked up on `PATH`, or set explicitly with
/// `AGENTD_SANDBOX_HELPER`.
const HELPER: &str = "agentd-sandbox-helper";

/// The environment variable that overrides where the Landlock helper is found.
const HELPER_ENV: &str = "AGENTD_SANDBOX_HELPER";

/// The child-side egress forwarder binary looked up on `PATH`, or set explicitly
/// with `AGENTD_EGRESS_FORWARD`.
const FORWARD: &str = "agentd-egress-forward";

/// The environment variable that overrides where the forwarder is found.
const FORWARD_ENV: &str = "AGENTD_EGRESS_FORWARD";

/// The host roots a confined shell and its binaries need, bound read-only and
/// executable on Linux. `/bin` and `/lib` may be symlinks into `/usr`, which
/// Landlock resolves. `/nix` and `/run/current-system` cover NixOS, where the
/// shell and its shared libraries live in the store rather than under `/usr`.
const SYSTEM_ROOTS: &[&str] = &[
    "/bin",
    "/sbin",
    "/lib",
    "/lib32",
    "/lib64",
    "/libx32",
    "/usr",
    "/etc",
    "/nix",
    "/run/current-system",
];

/// Whether bubblewrap is installed and trusted, so a private network namespace
/// can be requested.
///
/// The egress model depends on it: a namespace gives the child a real boundary
/// (loopback and no IP route) and lets a Unix-socket proxy cross it, which is
/// what the proxy transport is chosen from (see `docs/egress.md`). True when
/// bubblewrap is `PATH`-resolvable and trusted; a repository cannot supply the
/// binary that builds the boundary, so this applies the same write-root rule as
/// the executor. Taking the policy is what keeps the two probes from disagreeing:
/// a `bwrap` inside a write root is unusable here *and* rejected by
/// [`select_backend`], so the transport choice cannot pick a namespace the
/// executor then refuses.
///
/// This is a *presence* probe, not a capability test: it does not try to build a
/// namespace, so a host that has bubblewrap but forbids unprivileged user
/// namespaces still reports `true`, and the failure surfaces when the command
/// runs. Probing for real would mean executing `bwrap` on every construction.
#[must_use]
pub fn bubblewrap_available(fs_policy: &FsPolicy) -> bool {
    find_on_path(BWRAP, Some(fs_policy)).is_some()
}

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
    /// The shell the command runs under, resolved on this host.
    shell: PathBuf,
    /// The egress forwarder, when the command reaches the proxy through a
    /// mounted Unix socket instead of an IP route.
    forward: Option<Forward>,
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

/// The child-side egress forwarder, when the proxy is a Unix socket the command
/// reaches from inside a private network namespace.
struct Forward {
    /// The forwarder binary, resolved on `PATH`.
    binary: PathBuf,
    /// The daemon proxy's Unix socket, bind-mounted in and read inside.
    socket: PathBuf,
    /// The loopback port the forwarder listens on (the `HTTP_PROXY` port).
    port: u16,
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
        let shell = process::resolve_shell(&policy.fs);
        let resolved =
            resolve_entries(&policy.fs, &workdir, &scratch, &path_dirs).inspect_err(|_| {
                let _ = fs::remove_dir_all(&scratch);
            })?;
        let backend =
            select_backend(policy, forced, &resolved, &scratch, &shell).inspect_err(|_| {
                let _ = fs::remove_dir_all(&scratch);
            })?;
        // A Unix-socket proxy is reached through the forwarder; a loopback TCP
        // proxy (or no proxy) needs none.
        let forward = match &policy.network.proxy {
            Some(proxy) if proxy.socket.is_some() => {
                let socket = proxy.socket.clone().unwrap_or_default();
                let binary = find_forwarder(&policy.fs).inspect_err(|_| {
                    let _ = fs::remove_dir_all(&scratch);
                })?;
                Some(Forward {
                    binary,
                    socket,
                    port: proxy.port,
                })
            },
            _ => None,
        };
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
            shell,
            forward,
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
                    .arg(&self.shell)
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
        // With a Unix-socket proxy, mount the socket in (as an option, before
        // `--`) and run the command under the forwarder, so every
        // `127.0.0.1:<port>` connection reaches the daemon proxy even though the
        // namespace has no IP route. Without a forwarder the shell runs the
        // command directly.
        if let Some(forward) = &self.forward {
            std_command
                .arg("--ro-bind")
                .arg(&forward.socket)
                .arg(&forward.socket);
        }
        std_command.arg("--chdir").arg(&self.workdir).arg("--");
        match &self.forward {
            Some(forward) => {
                std_command
                    .arg(&forward.binary)
                    .arg("--port")
                    .arg(forward.port.to_string())
                    .arg("--socket")
                    .arg(&forward.socket)
                    .arg("--")
                    .arg(&self.shell)
                    .arg("-c")
                    .arg(command);
            },
            None => {
                std_command.arg(&self.shell).arg("-c").arg(command);
            },
        }
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

/// Chooses the backend: a forced one, else the model the policy asks for.
///
/// The network model decides this (see `docs/egress.md`):
///
/// - **Loopback alone** is a *private network namespace*: only loopback exists,
///   so there is no egress path at all. That is bubblewrap's `--unshare-all`;
///   without `bwrap` construction fails closed.
/// - **A Unix-socket proxy** (the Linux form, chosen by the daemon whenever it
///   can) also runs in that private namespace: the socket crosses it as a bind
///   mount, so the child still has no IP route and needs no port filter.
/// - **A loopback TCP proxy** (the macOS-style form, on a host with no
///   namespace) needs the *shared* network with only the proxy port (and, if
///   `loopback` is also granted, the ephemeral range for the command's own
///   server) open. Only the Landlock helper can filter by port; without it
///   construction fails closed.
/// - Neither: prefer bubblewrap (namespaces plus mounts) and fall back to
///   Landlock for the filesystem and seccomp.
///
/// `Policy::validate` allows `loopback` with `proxy`; the pairing that matters
/// is the proxy's own form: a Unix-socket proxy takes the private namespace, and
/// only a loopback-TCP proxy takes the shared network with the port filter.
fn select_backend(
    policy: &Policy,
    forced: Option<ForcedBackend>,
    resolved: &Resolved,
    scratch: &Path,
    shell: &Path,
) -> Result<Backend, SandboxError> {
    match forced {
        // The test-only forced backends bypass the network-model guards on
        // purpose: they render args without the binary installed. They must stay
        // `#[cfg(test)]`, or a network policy would silently get the wrong model.
        Some(ForcedBackend::Bubblewrap(bwrap)) => Ok(Backend::Bubblewrap(bwrap)),
        Some(ForcedBackend::Landlock(helper)) => {
            landlock_backend(policy, &helper, resolved, scratch, shell)
        },
        None => {
            // A Unix-socket proxy needs a private namespace (no IP route) and
            // the forwarder; a loopback-only grant needs the same namespace.
            let unix_proxy = policy
                .network
                .proxy
                .as_ref()
                .is_some_and(|proxy| proxy.socket.is_some());
            let private_namespace =
                (policy.network.loopback && policy.network.proxy.is_none()) || unix_proxy;
            if private_namespace {
                return find_on_path(BWRAP, Some(&policy.fs))
                    .map(Backend::Bubblewrap)
                    .ok_or_else(|| {
                        SandboxError::InvalidPolicy(String::from(
                            "the policy needs a private network namespace, which requires \
                             bubblewrap; bubblewrap is unavailable",
                        ))
                    });
            }
            // A loopback TCP proxy has no namespace to confine it, so it needs
            // the Landlock port filter.
            if policy.network.proxy.is_some() {
                let helper = find_helper(&policy.fs).map_err(|_| {
                    SandboxError::InvalidPolicy(String::from(
                        "the policy grants a loopback proxy, which requires the Landlock helper; \
                         bubblewrap cannot filter by port and the helper is unavailable",
                    ))
                })?;
                return landlock_backend(policy, &helper, resolved, scratch, shell);
            }
            if let Some(bwrap) = find_on_path(BWRAP, Some(&policy.fs)) {
                return Ok(Backend::Bubblewrap(bwrap));
            }
            let helper = find_helper(&policy.fs)?;
            landlock_backend(policy, &helper, resolved, scratch, shell)
        },
    }
}

/// Writes the Landlock spec and returns the helper backend.
fn landlock_backend(
    policy: &Policy,
    helper: &Path,
    resolved: &Resolved,
    scratch: &Path,
    shell: &Path,
) -> Result<Backend, SandboxError> {
    ensure_denies_enforceable(resolved, scratch, shell)?;
    let spec = build_spec(policy, resolved, scratch, shell);
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
    shell: &Path,
) -> Result<(), SandboxError> {
    let granted = granted_roots(resolved, scratch, shell);
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

/// The unmerged path rules the policy grants, in a fixed order.
///
/// [`build_spec`] merges overlapping paths to the maximal access; this list is
/// also the source [`granted_roots`] checks a `deny` against, so the two cannot
/// drift (a `deny` under any granted root is unenforceable by Landlock).
fn policy_rules(
    resolved: &Resolved,
    scratch: &Path,
    shell: &Path,
) -> Vec<PathRule> {
    let mut paths: Vec<PathRule> = Vec::new();
    for root in SYSTEM_ROOTS {
        push_existing(&mut paths, PathBuf::from(root), PathAccess::ReadExecute);
    }
    // The resolved shell may live outside every system root (e.g. a per-user
    // profile); grant its directory read-execute so the command can start.
    if let Some(parent) = shell.parent() {
        push_existing(&mut paths, parent.to_path_buf(), PathAccess::ReadExecute);
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
    paths
}

/// The canonical paths the Landlock allowlist grants, for deny comparison.
fn granted_roots(
    resolved: &Resolved,
    scratch: &Path,
    shell: &Path,
) -> Vec<PathBuf> {
    policy_rules(resolved, scratch, shell)
        .into_iter()
        .filter_map(|rule| rule.path.canonicalize().ok())
        .collect()
}

/// Renders the resolved policy into a Landlock spec.
///
/// System roots and devices a shell needs are granted; read entries are granted
/// read-only; write roots and the scratch directory are granted read-write;
/// `deny` entries are simply omitted, because Landlock cannot subtract. Two
/// rules on the same path merge to the *maximal* access, so a grant cannot
/// silently weaken a write root that happens to hold the shell.
fn build_spec(
    policy: &Policy,
    resolved: &Resolved,
    scratch: &Path,
    shell: &Path,
) -> Spec {
    let mut paths = policy_rules(resolved, scratch, shell);
    paths.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| right.access.rank().cmp(&left.access.rank()))
    });
    paths.dedup_by(|left, right| left.path == right.path);

    // Landlock port rules are for the *shared* network (a loopback TCP proxy on
    // a host with no namespace, macOS-style). A Unix-socket proxy runs in a
    // private namespace with no IP route, so it needs none.
    let loopback_proxy = policy
        .network
        .proxy
        .as_ref()
        .is_some_and(|proxy| proxy.socket.is_none());
    let (bind_ports, connect_ports) = if loopback_proxy {
        let proxy_port = policy.network.proxy.as_ref().map_or(0, |proxy| proxy.port);
        let mut connect_ports = vec![proxy_port];
        let mut bind_ports = Vec::new();
        if policy.network.loopback {
            bind_ports.push(0);
            connect_ports.extend(ephemeral_range());
        }
        connect_ports.sort_unstable();
        connect_ports.dedup();
        (bind_ports, connect_ports)
    } else {
        (Vec::new(), Vec::new())
    };
    Spec {
        paths,
        bind_ports,
        connect_ports,
    }
}

/// The host's ephemeral port range, from `ip_local_port_range` (default
/// 32768-60999).
fn ephemeral_range() -> std::ops::RangeInclusive<u16> {
    let fallback = 32768..=60999;
    let Ok(text) = fs::read_to_string("/proc/sys/net/ipv4/ip_local_port_range") else {
        return fallback;
    };
    let mut parts = text.split_whitespace();
    let (Some(low), Some(high)) = (parts.next(), parts.next()) else {
        return fallback;
    };
    match (low.parse::<u16>(), high.parse::<u16>()) {
        (Ok(low), Ok(high)) if low <= high => low..=high,
        _ => fallback,
    }
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
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| {
        let candidate = dir.join(name);
        if !process::is_executable(&candidate) {
            return None;
        }
        let trusted =
            fs_policy.is_none_or(|policy| process::is_outside_write_roots(&candidate, policy));
        trusted.then_some(candidate)
    })
}

/// Finds the Landlock helper: the `AGENTD_SANDBOX_HELPER` override, a sibling of
/// the running binary, or `PATH`.
fn find_helper(fs_policy: &FsPolicy) -> Result<PathBuf, SandboxError> {
    match find_tool(HELPER, HELPER_ENV, fs_policy) {
        Find::Found(path) => Ok(path),
        Find::Untrusted => Err(SandboxError::UnsupportedPlatform(
            "the `agentd-sandbox-helper` exists but sits inside a policy write root, so it \
             cannot be trusted to build the boundary (a workspace must not supply it); choose \
             a --workdir that does not contain the helper, or point AGENTD_SANDBOX_HELPER at a \
             copy outside every write root",
        )),
        Find::Missing => Err(SandboxError::UnsupportedPlatform(
            "no confinement backend: bubblewrap is not installed and the `agentd-sandbox-helper` \
             was not found next to the daemon or on PATH",
        )),
    }
}

/// Finds the egress forwarder: the `AGENTD_EGRESS_FORWARD` override, a sibling
/// of the running binary, or `PATH`.
///
/// A Unix-socket proxy is reached through it, so it must be resolvable; a
/// repository cannot supply it (the same trust rule as the helper).
fn find_forwarder(fs_policy: &FsPolicy) -> Result<PathBuf, SandboxError> {
    match find_tool(FORWARD, FORWARD_ENV, fs_policy) {
        Find::Found(path) => Ok(path),
        Find::Untrusted => Err(SandboxError::InvalidPolicy(String::from(
            "the `agentd-egress-forward` exists but sits inside a policy write root, so it \
             cannot be trusted to bridge egress (a workspace must not supply it); choose a \
             --workdir that does not contain it, or point AGENTD_EGRESS_FORWARD at a copy \
             outside every write root",
        ))),
        Find::Missing => Err(SandboxError::InvalidPolicy(String::from(
            "the policy grants a tunnelled egress, which requires the `agentd-egress-forward` \
             binary next to the daemon or on PATH",
        ))),
    }
}

/// The outcome of looking for a companion binary.
enum Find {
    /// Found and trusted.
    Found(PathBuf),
    /// An explicit override that is not an executable outside the workspace.
    Untrusted,
    /// Not found anywhere.
    Missing,
}

/// Resolves a companion binary the daemon spawns (`bwrap` is found differently:
/// it is a system tool, not a sibling).
///
/// Order: the `env` override, then a **sibling of the running binary** (the
/// binary this crate builds sits next to the daemon), then `PATH`. A repository
/// could otherwise supply the very binary that builds the boundary, so every
/// candidate must be an executable outside a policy write root.
///
/// An override that is not a trusted executable is [`Find::Untrusted`] at once:
/// the operator named it, so silently resolving something else would be a
/// surprise. For the sibling the search continues, because a copy installed on
/// `PATH` outside the workspace is still a legitimate answer; but if nothing
/// trusted is found and a sibling did exist under a write root, the result is
/// [`Find::Untrusted`] rather than [`Find::Missing`], so the caller reports the
/// real reason (a workspace that contains the helper) instead of claiming the
/// binary is absent. A `cargo` tree under the workdir is exactly that shape.
fn find_tool(
    name: &str,
    env: &str,
    fs_policy: &FsPolicy,
) -> Find {
    find_tool_in(
        env,
        fs_policy,
        sibling_of_current_exe(name),
        find_on_path(name, Some(fs_policy)),
    )
}

/// The body of [`find_tool`], with the sibling and `PATH` candidates injected so
/// the search order and its reporting are testable without controlling the
/// running executable or the process environment.
fn find_tool_in(
    env: &str,
    fs_policy: &FsPolicy,
    sibling: Option<PathBuf>,
    on_path: Option<PathBuf>,
) -> Find {
    if let Some(configured) = std::env::var_os(env) {
        let candidate = PathBuf::from(configured);
        return if process::is_executable(&candidate)
            && process::is_outside_write_roots(&candidate, fs_policy)
        {
            Find::Found(candidate)
        } else {
            Find::Untrusted
        };
    }
    // The sibling wins when it is trusted; otherwise the search continues, but
    // its rejection is remembered so an empty result can say "untrusted" rather
    // than "missing".
    let (trusted_sibling, rejected_sibling) = sibling.map_or((None, false), |sibling| {
        if process::is_outside_write_roots(&sibling, fs_policy) {
            (Some(sibling), false)
        } else {
            (None, true)
        }
    });
    if let Some(sibling) = trusted_sibling {
        return Find::Found(sibling);
    }
    if let Some(found) = on_path {
        return Find::Found(found);
    }
    if rejected_sibling {
        return Find::Untrusted;
    }
    Find::Missing
}

/// The `name` binary next to the running executable.
///
/// A binary built by `cargo` sits beside the daemon in `target/<profile>`, but a
/// test runs from `target/<profile>/deps`, so both the executable's directory
/// and its parent are tried.
fn sibling_of_current_exe(name: &str) -> Option<PathBuf> {
    sibling_of(&std::env::current_exe().ok()?, name)
}

/// The `name` binary next to `exe`, or one directory above it.
///
/// Split from [`sibling_of_current_exe`] so the lookup is testable without a
/// real `current_exe`.
fn sibling_of(
    exe: &Path,
    name: &str,
) -> Option<PathBuf> {
    let dir = exe.parent()?;
    let candidate = dir.join(name);
    if process::is_executable(&candidate) {
        return Some(candidate);
    }
    let candidate = dir.parent()?.join(name);
    process::is_executable(&candidate).then_some(candidate)
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
    use crate::policy::{
        Access, EnvVar, FsEntry, FsPolicy, HostPort, Policy, ProxyGrant, ShellPolicy,
    };
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
    ///
    /// A unit test runs from `target/<profile>/deps`, but the binary is built
    /// into `target/<profile>`, so both are tried.
    fn helper_path() -> PathBuf {
        if let Some(path) = option_env!("CARGO_BIN_EXE_agentd-sandbox-helper") {
            return PathBuf::from(path);
        }
        let mut dir = std::env::current_exe().expect("the test binary has a path");
        dir.pop();
        let mut candidate = dir.join("agentd-sandbox-helper");
        if !candidate.exists() {
            dir.pop();
            candidate = dir.join("agentd-sandbox-helper");
        }
        candidate
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
    fn bwrap_masks_only_an_explicit_deny() {
        let dir = TempDir::new().expect("tempdir");
        let host = dir.path().canonicalize().expect("canonical tempdir");
        let policy = workdir_policy(&host);
        let executor =
            ConfinedProcessExecutor::with_bwrap(&policy, PathBuf::from("/usr/bin/bwrap"))
                .expect("the policy renders");

        // Masking the temp directory hid a granted read that lives under it —
        // and the daemon socket, which defaults to `$TMPDIR/agentd/agentd.sock`
        // when `XDG_RUNTIME_DIR` is unset — and left `/tmp` writable inside the
        // sandbox where the Landlock fallback leaves it outside the allowlist.
        // Reads are broad, so a read entry needs no mount of its own and a
        // `deny` is the only reason to mask a path.
        let plain = args(&executor.std_command("true"));
        assert!(
            !window(&plain, &["--tmpfs", "/tmp"]),
            "the temp directory must not be masked: {plain:?}"
        );

        // The one mask that remains is the named deny, which is what the
        // invariant above is about.
        let denied = host.join("secret");
        std::fs::create_dir(&denied).expect("deny dir");
        let policy = Policy {
            fs: FsPolicy {
                entries: vec![
                    FsEntry {
                        path: host.clone(),
                        access: Access::Write,
                    },
                    FsEntry {
                        path: denied.clone(),
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
        let executor =
            ConfinedProcessExecutor::with_bwrap(&policy, PathBuf::from("/usr/bin/bwrap"))
                .expect("the policy renders");
        let masked = args(&executor.std_command("true"));
        assert!(
            window(&masked, &["--tmpfs", &denied.display().to_string()]),
            "a deny must still be masked: {masked:?}"
        );
        assert!(
            !window(&masked, &["--tmpfs", "/tmp"]),
            "the temp directory must not be masked: {masked:?}"
        );
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
    fn the_proxy_grant_requires_the_landlock_helper() {
        let dir = TempDir::new().expect("tempdir");
        let host = dir.path().canonicalize().expect("canonical tempdir");
        let mut policy = workdir_policy(&host);
        policy.network.proxy = Some(crate::policy::ProxyGrant {
            port: 9000,
            socket: None,
            egress: vec![crate::policy::HostPort {
                host: String::from("api.example.com"),
                port: 443,
            }],
        });

        // The proxy needs the port filter the helper provides; bubblewrap cannot
        // express it. Construction succeeds when the helper is installed.
        match ConfinedProcessExecutor::new(&policy) {
            Ok(executor) => {
                let Backend::Landlock { spec_path, .. } = &executor.backend else {
                    panic!("a proxy grant must select the Landlock backend");
                };
                let spec: Spec =
                    serde_json::from_slice(&std::fs::read(spec_path).expect("the spec is written"))
                        .expect("the spec parses");
                assert_eq!(spec.connect_ports, vec![9000]);
            },
            Err(crate::error::SandboxError::InvalidPolicy(message)) => {
                assert!(
                    message.contains("helper"),
                    "the failure must explain the missing helper: {message}"
                );
            },
            Err(other) => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn the_loopback_grant_uses_bubblewrap_and_fails_closed_without_it() {
        let dir = TempDir::new().expect("tempdir");
        let host = dir.path().canonicalize().expect("canonical tempdir");
        let mut policy = workdir_policy(&host);
        policy.network.loopback = true;

        // `ConfinedProcessExecutor::new` reaches the real backend selection.
        // Either bubblewrap is present (bubblewrap backend) or the policy is
        // rejected for lacking it; it must never silently fall back to Landlock,
        // which cannot confine free loopback.
        match ConfinedProcessExecutor::new(&policy) {
            Ok(executor) => {
                assert!(
                    matches!(executor.backend, Backend::Bubblewrap(_)),
                    "a loopback grant must select the bubblewrap backend"
                );
            },
            Err(crate::error::SandboxError::InvalidPolicy(message)) => {
                assert!(
                    message.contains("bubblewrap"),
                    "the failure must explain the missing bubblewrap: {message}"
                );
            },
            Err(other) => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn landlock_spec_carries_the_proxy_port() {
        let dir = TempDir::new().expect("tempdir");
        let host = dir.path().canonicalize().expect("canonical tempdir");
        let mut policy = workdir_policy(&host);
        policy.network.proxy = Some(crate::policy::ProxyGrant {
            port: 9000,
            socket: None,
            egress: Vec::new(),
        });
        let executor =
            ConfinedProcessExecutor::with_landlock(&policy, helper_path()).expect("landlock spec");

        let Backend::Landlock { spec_path, .. } = &executor.backend else {
            panic!("the executor must use the Landlock backend");
        };
        let spec: Spec =
            serde_json::from_slice(&std::fs::read(spec_path).expect("the spec is written"))
                .expect("the spec parses");

        assert_eq!(spec.connect_ports, vec![9000]);
    }

    #[test]
    fn loopback_with_a_proxy_opens_the_ephemeral_range() {
        let dir = TempDir::new().expect("tempdir");
        let host = dir.path().canonicalize().expect("canonical tempdir");
        let mut policy = workdir_policy(&host);
        policy.network.loopback = true;
        policy.network.proxy = Some(crate::policy::ProxyGrant {
            port: 9000,
            socket: None,
            egress: Vec::new(),
        });
        let executor =
            ConfinedProcessExecutor::with_landlock(&policy, helper_path()).expect("landlock spec");

        let Backend::Landlock { spec_path, .. } = &executor.backend else {
            panic!("the executor must use the Landlock backend");
        };
        let spec: Spec =
            serde_json::from_slice(&std::fs::read(spec_path).expect("the spec is written"))
                .expect("the spec parses");

        // The proxy port and the whole ephemeral range are connectable, and the
        // ephemeral range is bindable via the one range form Landlock offers.
        assert_eq!(spec.connect_ports.first(), Some(&9000));
        assert_eq!(spec.connect_ports.last(), Some(&60999));
        assert_eq!(spec.bind_ports, vec![0]);
    }

    #[test]
    fn a_unix_socket_proxy_mounts_the_socket_and_runs_the_forwarder() {
        let dir = TempDir::new().expect("tempdir");
        let host = dir.path().canonicalize().expect("canonical tempdir");
        let socket = host.join("proxy.sock");
        std::fs::write(&socket, b"").expect("a socket path exists on the host");
        let mut policy = workdir_policy(&host);
        policy.network.proxy = Some(ProxyGrant {
            port: 31_828,
            socket: Some(socket.clone()),
            egress: vec![HostPort {
                host: String::from("api.example.com"),
                port: 443,
            }],
        });
        // Uses the bubblewrap backend directly, so the render does not depend on
        // `bwrap` being installed (the backend-selection path is covered
        // elsewhere).
        let executor =
            ConfinedProcessExecutor::with_bwrap(&policy, PathBuf::from("/usr/bin/bwrap"))
                .expect("the policy renders");

        let args = args(&executor.std_command("true"));
        let socket_arg = socket.display().to_string();
        // The socket crosses the private namespace as a bind mount.
        assert!(
            window(&args, &["--ro-bind", &socket_arg, &socket_arg]),
            "the proxy socket must be bind-mounted in: {args:?}"
        );
        // The forwarder runs as the command's parent, bridging loopback to it.
        assert!(
            window(&args, &["--port", "31828", "--socket", &socket_arg, "--"]),
            "the command must run under the forwarder: {args:?}"
        );
        // The namespace is still private: the forwarder is the only route out.
        assert!(args.iter().any(|arg| arg == "--unshare-all"));
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
    fn a_companion_is_found_next_to_the_binary() {
        // The installed layout puts `agentd`, its helper, and the forwarder in
        // one `bin/`; resolution must find the sibling so NixOS packaging needs
        // no PATH entry.
        use super::sibling_of;
        let dir = TempDir::new().expect("tempdir");
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).expect("bin dir");
        let exe = bin.join("agentd");
        std::fs::write(&exe, b"").expect("exe");
        let companion = bin.join("agentd-egress-forward");
        std::fs::write(&companion, b"").expect("companion");
        std::fs::set_permissions(
            &companion,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .expect("chmod");

        assert_eq!(sibling_of(&exe, "agentd-egress-forward"), Some(companion));
        assert_eq!(sibling_of(&exe, "agentd-sandbox-helper"), None);
    }

    #[test]
    fn a_companion_is_found_above_a_deps_directory() {
        // `cargo test` runs from `target/<profile>/deps`, so the sibling is the
        // parent's child.
        use super::sibling_of;
        let dir = TempDir::new().expect("tempdir");
        let profile = dir.path().join("profile");
        let deps = profile.join("deps");
        std::fs::create_dir_all(&deps).expect("deps dir");
        let exe = deps.join("agentd-sandbox-abc123");
        std::fs::write(&exe, b"").expect("exe");
        let companion = profile.join("agentd-sandbox-helper");
        std::fs::write(&companion, b"").expect("companion");
        std::fs::set_permissions(
            &companion,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .expect("chmod");

        assert_eq!(sibling_of(&exe, "agentd-sandbox-helper"), Some(companion));
    }

    /// A policy whose only write root is `root`, with a trusted home elsewhere.
    fn policy_writing(root: &Path) -> FsPolicy {
        FsPolicy {
            entries: vec![FsEntry {
                path: root.to_path_buf(),
                access: Access::Write,
            }],
            ..FsPolicy::default()
        }
    }

    /// Writes an executable file and returns its path.
    fn executable(path: &Path) -> PathBuf {
        std::fs::write(path, b"").expect("write");
        std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .expect("chmod");
        path.to_path_buf()
    }

    #[test]
    fn an_untrusted_sibling_is_reported_as_untrusted_not_missing() {
        // The regression: a `cargo` tree inside the workdir makes the daemon's
        // sibling helper sit inside a write root. Reporting that as "missing"
        // blamed bubblewrap for what is really a too-wide --workdir.
        use super::{Find, find_tool_in};
        let dir = TempDir::new().expect("tempdir");
        let workdir = dir.path().join("workspace");
        std::fs::create_dir_all(workdir.join("target/debug")).expect("tree");
        let sibling = executable(&workdir.join("target/debug/agentd-sandbox-helper"));

        let found = find_tool_in(
            "AGENTD_TEST_HELPER_UNSET",
            &policy_writing(&workdir),
            Some(sibling),
            None,
        );

        assert!(
            matches!(found, Find::Untrusted),
            "a rejected sibling must not read as missing"
        );
    }

    #[test]
    fn a_trusted_sibling_outside_the_write_root_is_found() {
        use super::{Find, find_tool_in};
        let dir = TempDir::new().expect("tempdir");
        let workdir = dir.path().join("workspace");
        std::fs::create_dir_all(&workdir).expect("workspace");
        let sibling_dir = dir.path().join("bin");
        std::fs::create_dir_all(&sibling_dir).expect("bin");
        let sibling = executable(&sibling_dir.join("agentd-sandbox-helper"));

        let found = find_tool_in(
            "AGENTD_TEST_HELPER_UNSET",
            &policy_writing(&workdir),
            Some(sibling.clone()),
            None,
        );

        assert!(matches!(found, Find::Found(path) if path == sibling));
    }

    #[test]
    fn a_rejected_sibling_still_allows_a_trusted_copy_on_path() {
        // A helper beside a too-wide workdir must not shadow one installed
        // outside it: the `PATH` fallback still resolves the trusted copy.
        use super::{Find, find_tool_in};
        let dir = TempDir::new().expect("tempdir");
        let workdir = dir.path().join("workspace");
        std::fs::create_dir_all(workdir.join("target/debug")).expect("tree");
        let rejected = executable(&workdir.join("target/debug/agentd-sandbox-helper"));
        let trusted = dir.path().join("trusted-bin").join("agentd-sandbox-helper");

        let found = find_tool_in(
            "AGENTD_TEST_HELPER_UNSET",
            &policy_writing(&workdir),
            Some(rejected),
            Some(trusted.clone()),
        );

        assert!(
            matches!(found, Find::Found(path) if path == trusted),
            "the trusted PATH copy must win over a rejected sibling"
        );
    }

    #[test]
    fn the_resolved_shell_is_executable() {
        // A non-FHS host (NixOS) keeps its shell under `/nix/store`; resolution
        // must find a runnable one rather than assume `/bin/bash`.
        let shell = crate::process::resolve_shell(&FsPolicy::default());
        assert!(
            shell.is_file(),
            "the resolved shell must exist: {}",
            shell.display()
        );
    }

    #[test]
    fn the_executor_confines_on_a_non_fhs_host() {
        // Regression guard: before shell resolution, the probe used `/bin/bash`,
        // which NixOS lacks, so every Landlock test silently skipped. When the
        // helper exists and the kernel can confine, the probe must now succeed.
        let dir = TempDir::new().expect("tempdir");
        let host = dir.path().canonicalize().expect("canonical tempdir");
        // `landlock_executor` itself probes the kernel and returns `None` on a
        // host it cannot confine, so `None` here is a skip, not a failure.
        let Some(executor) = landlock_executor(&workdir_policy(&host)) else {
            return;
        };

        let result = executor.blocking_exec("echo confined-ok");
        assert_eq!(result.exit_code, 0, "stderr: {}", result.stderr);
        assert!(result.stdout.contains("confined-ok"));
    }

    #[test]
    fn landlock_denies_tcp_when_no_network_is_granted() {
        // Regression guard: network must be *handled* even when no port is
        // granted, or Landlock leaves TCP entirely unrestricted.
        let dir = TempDir::new().expect("tempdir");
        let host = dir.path().canonicalize().expect("canonical tempdir");
        let Some(executor) = landlock_executor(&workdir_policy(&host)) else {
            return;
        };

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        std::thread::spawn(move || {
            let _ = listener.accept();
        });

        let result =
            executor.blocking_exec(&format!("exec 3<>/dev/tcp/127.0.0.1/{port} && echo open"));
        assert_ne!(
            result.exit_code, 0,
            "a session with no network grant must not connect: {}",
            result.stdout
        );
    }

    #[test]
    fn landlock_grants_only_the_named_connect_port() {
        if !helper_path().exists() {
            return;
        }
        // The connect probe uses bash's `/dev/tcp`; skip if the resolved shell
        // is not bash, before starting a listener and its accept thread.
        let resolved_is_bash = crate::process::resolve_shell(&FsPolicy::default())
            .file_name()
            .is_some_and(|name| name == "bash");
        if !resolved_is_bash {
            return;
        }
        // A listener the confined command may reach only on its port.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let allowed_port = listener.local_addr().expect("addr").port();
        let deny_port = {
            let other = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            other.local_addr().expect("addr").port()
        };
        std::thread::spawn(move || {
            let _ = listener.accept();
        });

        let dir = TempDir::new().expect("tempdir");
        let host = dir.path().canonicalize().expect("canonical tempdir");
        let mut policy = workdir_policy(&host);
        policy.network.proxy = Some(crate::policy::ProxyGrant {
            port: allowed_port,
            socket: None,
            egress: Vec::new(),
        });
        let Some(executor) = landlock_executor(&policy) else {
            return;
        };

        let connect = |port: u16| {
            executor.blocking_exec(&format!("exec 3<>/dev/tcp/127.0.0.1/{port} && echo open"))
        };

        // The proxy port is reachable; a different loopback port is denied.
        let allowed = connect(allowed_port);
        assert!(
            allowed.exit_code == 0 && allowed.stdout.contains("open"),
            "the proxy port must be reachable: {}",
            allowed.stderr
        );

        let denied = connect(deny_port);
        assert_ne!(
            denied.exit_code, 0,
            "a port outside the policy must be denied: {}",
            denied.stdout
        );
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
        //
        // The check uses shell builtins only. An earlier version piped through
        // `grep`, which is not guaranteed to be on the confined `PATH`: the Nix
        // build sandbox has no system `grep` where the fixed `PATH` looks, so
        // the test failed there for a reason unrelated to seccomp.
        let seccomp = executor.blocking_exec(
            "while read -r key value rest; do \
               if [ \"$key\" = \"Seccomp:\" ] && [ \"$value\" = \"2\" ]; then echo active; fi; \
             done < /proc/self/status",
        );
        assert_eq!(seccomp.exit_code, 0, "stderr: {}", seccomp.stderr);
        assert_eq!(
            seccomp.stdout.trim(),
            "active",
            "the seccomp filter must be installed, seen through /proc/self/status: {}",
            seccomp.stdout
        );
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
