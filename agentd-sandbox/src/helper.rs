//! The Linux confinement helper: a small executable that applies a Landlock
//! ruleset and a seccomp filter to itself, then `exec`s the confined command.
//!
//! Applying confinement to a child process without the `unsafe` `pre_exec` that
//! the workspace forbids needs a separate program: the executor serializes a
//! [`Spec`] to a file in its scratch directory and invokes this helper, which is
//! the only process that calls the confinement syscalls. The helper is a
//! dedicated binary rather than an `argv[0]` overloading of the daemon, because
//! a separate binary is directly testable and needs no daemon wiring.
//!
//! The helper is invoked as `agentd-sandbox-helper <spec> <program> [args...]`.
//! It fails closed: any setup error exits non-zero before `exec`, so the
//! command never runs unconfined.

use std::ffi::OsString;
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use landlock::{
    ABI, Access, AccessFs, AccessNet, CompatLevel, Compatible, NetPort, PathBeneath, PathFd,
    Ruleset, RulesetAttr, RulesetCreatedAttr,
};
use seccompiler::{BpfProgram, SeccompAction, SeccompFilter, SeccompRule, TargetArch};
use serde::{Deserialize, Serialize};

/// The confinement the helper applies.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Spec {
    /// The path rules, evaluated as an allowlist.
    pub paths: Vec<PathRule>,
    /// The TCP ports the command may bind. `0` means the ephemeral range
    /// (`/proc/sys/net/ipv4/ip_local_port_range`), which is the one form
    /// Landlock gives a range for. Empty denies all TCP bind.
    pub bind_ports: Vec<u16>,
    /// The TCP ports the command may connect to. Landlock has no port-range
    /// rule for connect (`0` covers bind only), so an ephemeral range is a long
    /// list; the kernel accepts ~28k rules in a few milliseconds. Empty denies
    /// all TCP connect.
    pub connect_ports: Vec<u16>,
}

/// One Landlock path rule.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct PathRule {
    /// The absolute host path.
    pub path: PathBuf,
    /// The access granted on `path` and everything under it.
    pub access: PathAccess,
}

/// The access a [`PathRule`] grants.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PathAccess {
    /// Read files and list directories.
    Read,
    /// Read and execute, for the system directories a shell needs.
    ReadExecute,
    /// Read, write, and execute.
    Write,
}

impl PathAccess {
    /// A total order for merging two rules on the same path: a higher rank
    /// grants strictly more, so the maximum wins.
    pub(crate) const fn rank(self) -> u8 {
        match self {
            Self::Read => 0,
            Self::ReadExecute => 1,
            Self::Write => 2,
        }
    }
}

/// The syscalls the helper refuses with `EPERM`.
///
/// This is a deny-list, a defence in depth on top of Landlock and the network
/// namespace: the calls that escape a sandbox or smuggle kernel access. It does
/// not block ordinary commands, so it cannot be observed from a shell other
/// than through `/proc/self/status`.
pub(crate) const BLOCKED_SYSCALLS: &[i64] = &[
    libc::SYS_ptrace,
    libc::SYS_process_vm_readv,
    libc::SYS_process_vm_writev,
    libc::SYS_io_uring_setup,
    libc::SYS_io_uring_enter,
    libc::SYS_io_uring_register,
    libc::SYS_bpf,
    libc::SYS_userfaultfd,
    libc::SYS_keyctl,
    libc::SYS_add_key,
    libc::SYS_request_key,
    libc::SYS_kexec_load,
    libc::SYS_open_by_handle_at,
    libc::SYS_perf_event_open,
];

/// The helper entry point: reads the spec, confines itself, and `exec`s.
///
/// Never returns on success — `exec` replaces the process. On failure it prints
/// the reason and returns a non-zero code, so the command is not run.
#[must_use]
pub fn run() -> ExitCode {
    let mut args = std::env::args_os();
    let _program = args.next();
    let Some(spec_path) = args.next() else {
        eprintln!(
            "[agentd-sandbox-helper] usage: agentd-sandbox-helper <spec> <program> [args...]"
        );
        return ExitCode::FAILURE;
    };
    let command: Vec<OsString> = args.collect();
    if let Err(error) = confine(Path::new(&spec_path), &command) {
        eprintln!("[agentd-sandbox-helper] {error}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

/// Reads `spec_path`, applies the confinement, and replaces the process with
/// `command`.
///
/// # Errors
///
/// Returns a message when the spec cannot be read or parsed, a rule cannot be
/// applied, or the command is empty or cannot be executed.
pub(crate) fn confine(
    spec_path: &Path,
    command: &[OsString],
) -> Result<(), String> {
    let raw = fs::read(spec_path).map_err(|error| format!("cannot read the spec: {error}"))?;
    let spec: Spec =
        serde_json::from_slice(&raw).map_err(|error| format!("cannot parse the spec: {error}"))?;
    apply_landlock(&spec)?;
    apply_seccomp(BLOCKED_SYSCALLS)?;
    let (program, args) = command.split_first().ok_or("no command to run")?;
    let error = std::process::Command::new(program).args(args).exec();
    Err(format!(
        "cannot execute {}: {error}",
        program.to_string_lossy()
    ))
}

/// Applies the Landlock allowlist ruleset to the current thread and its
/// children.
///
/// Filesystem access is allowlisted by the rules. Network access is *handled*
/// but never allowed unless the spec names a port, so TCP bind and connect are
/// denied by default; `AF_UNIX` sockets are filesystem objects and are
/// unaffected.
///
/// # Errors
///
/// Network handling needs Landlock ABI v4 (Linux 6.7). On an older kernel the
/// crate would drop it silently under its default best-effort mode and each
/// `NetPort` rule would become a no-op, so a spec that grants a network port
/// would run **unfiltered**. When the spec asks for network access, this fails
/// closed instead: the net access is requested as a
/// [`CompatLevel::HardRequirement`], so a kernel that cannot enforce it makes
/// `handle_access` return an error before the command runs.
///
/// A filesystem restriction can also be *partially* enforced, which is fine and
/// unrelated; the network guarantee is what the hard requirement pins.
fn apply_landlock(spec: &Spec) -> Result<(), String> {
    let abi = ABI::V1;
    let handled = AccessFs::from_all(abi);
    // Network is always handled, so TCP bind and connect are deny-by-default:
    // *not* handling an access class leaves it entirely unrestricted. Only when
    // a port is granted does the handling need to be a hard requirement, so a
    // kernel without ABI v4 fails closed instead of silently ignoring it.
    let net = AccessNet::from_all(ABI::V4);
    let wants_network = !spec.bind_ports.is_empty() || !spec.connect_ports.is_empty();
    let level = if wants_network {
        CompatLevel::HardRequirement
    } else {
        CompatLevel::BestEffort
    };
    let ruleset = Ruleset::default()
        .handle_access(handled)
        .map_err(|error| format!("cannot create the Landlock ruleset: {error}"))?
        .set_compatibility(level)
        .handle_access(net)
        .map_err(|error| {
            format!(
                "the kernel cannot enforce Landlock network rules (needs ABI v4, \
                 Linux 6.7): {error}"
            )
        })?
        // Reset to best-effort so the per-path rules below are not made strict
        // about access rights a plain file cannot carry.
        .set_compatibility(CompatLevel::BestEffort);
    let mut ruleset = ruleset
        .create()
        .map_err(|error| format!("cannot create the Landlock ruleset: {error}"))?;
    for rule in &spec.paths {
        let access = access_for(rule.access, abi);
        if !rule.path.exists() {
            // Landlock cannot express a missing path; the executor only lists
            // paths it verified, so a vanished path grants nothing.
            continue;
        }
        let fd = PathFd::new(&rule.path)
            .map_err(|error| format!("cannot open {:?}: {error}", rule.path.display()))?;
        ruleset = ruleset
            .add_rule(PathBeneath::new(fd, access))
            .map_err(|error| format!("cannot add a rule for {:?}: {error}", rule.path.display()))?;
    }
    // A port rule is port-only: Landlock has no host dimension, so granting
    // port `P` permits bind or connect on `P` on any address, not only loopback
    // (see `docs/egress.md`). With no port named, TCP stays denied.
    for port in &spec.bind_ports {
        ruleset = ruleset
            .add_rule(NetPort::new(*port, AccessNet::BindTcp))
            .map_err(|error| format!("cannot allow bind on port {port}: {error}"))?;
    }
    for port in &spec.connect_ports {
        ruleset = ruleset
            .add_rule(NetPort::new(*port, AccessNet::ConnectTcp))
            .map_err(|error| format!("cannot allow connect on port {port}: {error}"))?;
    }
    ruleset
        .restrict_self()
        .map_err(|error| format!("cannot enforce the Landlock ruleset: {error}"))?;
    Ok(())
}

/// Maps a policy access to the Landlock rights it grants.
fn access_for(
    access: PathAccess,
    abi: ABI,
) -> landlock::BitFlags<AccessFs> {
    match access {
        PathAccess::Read => AccessFs::from_read(abi),
        // `from_read` does not include `Execute`, which a shell needs to run a
        // binary from a system directory.
        PathAccess::ReadExecute => {
            AccessFs::from_read(abi) | landlock::BitFlags::from(AccessFs::Execute)
        },
        PathAccess::Write => AccessFs::from_all(abi),
    }
}

/// Installs the seccomp filter that refuses `blocked` with `EPERM`.
fn apply_seccomp(blocked: &[i64]) -> Result<(), String> {
    let rules = blocked
        .iter()
        .map(|syscall| (*syscall, Vec::<SeccompRule>::new()))
        .collect();
    let arch = TargetArch::try_from(std::env::consts::ARCH)
        .map_err(|error| format!("unsupported architecture: {error}"))?;
    let filter: BpfProgram = SeccompFilter::new(
        rules,
        SeccompAction::Allow,
        SeccompAction::Errno(libc::EPERM.unsigned_abs()),
        arch,
    )
    .map_err(|error| format!("cannot build the seccomp filter: {error}"))?
    .try_into()
    .map_err(|error| format!("cannot compile the seccomp filter: {error}"))?;
    seccompiler::apply_filter(&filter)
        .map_err(|error| format!("cannot install the seccomp filter: {error}"))
}
