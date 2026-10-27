//! The bounds a tool's process starts inside.
//!
//! A tool is a program the operator installed and a model asked for. What it may
//! touch is written in its manifest, and this module is where the kernel is told:
//! Landlock refuses the writes that were not declared, seccomp refuses the sockets
//! a tool did not ask for, and two resource limits hold its memory and the size of
//! one file.
//!
//! None of it needs root, a namespace, or a program from outside this package. The
//! bounds are syscalls made by the process that is about to become the tool, which
//! is why they hold where a sandbox that needs namespaces cannot run at all.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use landlock::{
    ABI, AccessFs, PathBeneath, PathFd, Ruleset, RulesetAttr as _, RulesetCreatedAttr as _,
    RulesetStatus,
};
use rustix::process::{Resource, Rlimit};

use crate::{Network, Settings, Tool};

/// The argument the launcher answers to prove it can install the bounds here.
///
/// It is the same code with nothing to exec afterwards, so a host that refuses a
/// bound says so at startup instead of on the first tool call.
pub const CHECK: &str = "--check";

/// What a tool may touch, as the launcher reads it from its own command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bounds {
    pub file_bytes: u64,
    pub memory_bytes: u64,
    pub network: Network,
    pub write: Vec<PathBuf>,
}

/// The whole command that starts a tool inside its bounds.
///
/// The launcher is this package's own second binary, so a host needs nothing
/// installed to bound a tool, and the daemon finds it beside itself.
#[must_use]
pub fn plan(
    launcher: &Path,
    tool: &Tool,
    program: &Path,
) -> Vec<OsString> {
    let mut argv: Vec<OsString> = vec![
        launcher.to_path_buf().into_os_string(),
        format!("--memory-bytes={}", tool.memory_bytes).into(),
        format!("--file-bytes={}", tool.file_bytes).into(),
        format!(
            "--network={}",
            match tool.network {
                Network::None => "none",
                Network::Any => "any",
            }
        )
        .into(),
    ];
    for directory in &tool.write {
        argv.push(format!("--write={}", directory.display()).into());
    }
    argv.push("--".into());
    // The program is resolved here rather than by the launcher, because a name
    // without a path is found through `PATH` and the tool's own environment has
    // none unless its manifest declares one.
    argv.push(program.to_path_buf().into_os_string());
    argv.extend(tool.argv.iter().skip(1).map(OsString::from));
    argv
}

/// Reads the bounds out of a launcher's own command line.
///
/// # Errors
///
/// Returns a sentence naming the argument when the command line is not one this
/// package wrote, because the launcher is not meant to be typed by hand.
pub fn bounds_from(arguments: &[OsString]) -> Result<(Bounds, Vec<OsString>), String> {
    let mut file_bytes = None;
    let mut memory_bytes = None;
    let mut network = None;
    let mut write = Vec::new();
    let mut rest = arguments.iter();

    while let Some(argument) = rest.next() {
        let word = argument
            .to_str()
            .ok_or_else(|| String::from("an argument is not text"))?;
        if word == "--" {
            let bounds = Bounds {
                file_bytes: file_bytes.ok_or("--file-bytes is missing")?,
                memory_bytes: memory_bytes.ok_or("--memory-bytes is missing")?,
                network: network.ok_or("--network is missing")?,
                write,
            };
            return Ok((bounds, rest.map(OsString::from).collect()));
        }
        let (flag, value) = word.split_once('=').ok_or_else(|| {
            format!("{word} is not an option the launcher takes, which the daemon writes")
        })?;
        match flag {
            "--file-bytes" => file_bytes = Some(number(value)?),
            "--memory-bytes" => memory_bytes = Some(number(value)?),
            "--network" => {
                network = Some(match value {
                    "none" => Network::None,
                    "any" => Network::Any,
                    other => return Err(format!("network is none or any, not {other}")),
                });
            },
            "--write" => write.push(PathBuf::from(value)),
            other => return Err(format!("{other} is not an option the launcher takes")),
        }
    }
    Err(String::from("the command line names no program"))
}

fn number(value: &str) -> Result<u64, String> {
    value
        .parse()
        .map_err(|_| format!("{value} is not a number of bytes"))
}

/// Puts the calling process inside `bounds`, and returns only if all of them hold.
///
/// The limits go first, because they are inherited by the program the caller execs
/// afterwards, so a tool cannot shed a limit by starting another program.
///
/// # Errors
///
/// Returns the kernel's own reason when a bound cannot be installed. A bound that
/// could not be installed is never reported as installed.
pub fn confine(bounds: &Bounds) -> Result<(), String> {
    limit(Resource::As, bounds.memory_bytes, "memory")?;
    limit(Resource::Fsize, bounds.file_bytes, "file")?;
    rustix::thread::set_no_new_privs(true).map_err(|error| format!("no_new_privs: {error}"))?;
    filesystem(bounds)?;
    if bounds.network == Network::None {
        network()?;
    }
    Ok(())
}

fn limit(
    resource: Resource,
    bytes: u64,
    what: &str,
) -> Result<(), String> {
    let limit = Rlimit {
        current: Some(bytes),
        maximum: Some(bytes),
    };
    rustix::process::setrlimit(resource, limit).map_err(|error| format!("{what} limit: {error}"))
}

/// Takes every write that the manifest did not declare.
///
/// The ruleset handles the whole write half of the filesystem, so a path with no
/// rule is refused rather than unnoticed: the declared directories are the only
/// places a rule exists for.
fn filesystem(bounds: &Bounds) -> Result<(), String> {
    // The third ABI is the first that covers truncation, and truncation is a
    // write: without it a tool could empty a file it may not otherwise touch. A
    // kernel older than that is refused rather than bounded by halves.
    let handled = AccessFs::from_write(ABI::V3);
    let mut ruleset = Ruleset::default()
        .handle_access(handled)
        .map_err(|error| format!("landlock ruleset: {error}"))?
        .create()
        .map_err(|error| format!("landlock: {error}"))?;
    for directory in &bounds.write {
        let path =
            PathFd::new(directory).map_err(|error| format!("{}: {error}", directory.display()))?;
        ruleset = ruleset
            .add_rule(PathBeneath::new(path, handled))
            .map_err(|error| format!("{}: {error}", directory.display()))?;
    }
    // Every program redirects to /dev/null, and it holds nothing, so refusing it
    // turns a working tool into one that reports failures it did not have. It is
    // the one path this build grants without being asked.
    if let Ok(null) = PathFd::new("/dev/null") {
        ruleset = ruleset
            .add_rule(PathBeneath::new(null, AccessFs::WriteFile))
            .map_err(|error| format!("/dev/null: {error}"))?;
    }
    let status = ruleset
        .restrict_self()
        .map_err(|error| format!("landlock: {error}"))?;
    if status.ruleset != RulesetStatus::FullyEnforced {
        return Err(String::from(
            "this kernel cannot enforce every write bound, so the tool was not started",
        ));
    }
    Ok(())
}

/// Takes the network away, and the sockets with it.
///
/// Landlock can refuse the calls a connection needs, but a name lookup, a datagram
/// and a raw packet each start with a socket. Refusing the address families a tool
/// would need to reach anything is one rule per family and covers all three.
fn network() -> Result<(), String> {
    let filter: seccompiler::BpfProgram = seccompiler::SeccompFilter::new(
        rules(),
        seccompiler::SeccompAction::Allow,
        seccompiler::SeccompAction::Errno(libc::EACCES.unsigned_abs()),
        arch(),
    )
    .map_err(|error| format!("seccomp: {error}"))?
    .try_into()
    .map_err(|error| format!("seccomp: {error}"))?;
    seccompiler::apply_filter(&filter).map_err(|error| format!("seccomp: {error}"))
}

/// One rule per address family that could reach another machine.
fn rules() -> std::collections::BTreeMap<i64, Vec<seccompiler::SeccompRule>> {
    let mut rules = std::collections::BTreeMap::new();
    let mut families = Vec::new();
    for family in [libc::AF_INET, libc::AF_INET6, libc::AF_PACKET] {
        let Ok(condition) = seccompiler::SeccompCondition::new(
            0,
            seccompiler::SeccompCmpArgLen::Dword,
            seccompiler::SeccompCmpOp::Eq,
            u64::try_from(family).unwrap_or(0),
        ) else {
            continue;
        };
        let Ok(rule) = seccompiler::SeccompRule::new(vec![condition]) else {
            continue;
        };
        families.push(rule);
    }
    rules.insert(libc::SYS_socket, families);
    rules
}

fn arch() -> seccompiler::TargetArch {
    arch_of(std::env::consts::ARCH)
}

/// The filter is built for the machine it will run on, named the way rustc names
/// it, so the choice can be read and tested without running on each of them.
fn arch_of(name: &str) -> seccompiler::TargetArch {
    match name {
        "aarch64" => seccompiler::TargetArch::aarch64,
        "riscv64" => seccompiler::TargetArch::riscv64,
        _ => seccompiler::TargetArch::x86_64,
    }
}

/// The launcher this package ships, found beside the program that is running.
///
/// A test points `MOKMOK_CONFINE` at the copy cargo built for it; a deployment
/// moves the two binaries together and says nothing.
///
/// # Errors
///
/// Returns a sentence naming the path when there is no launcher to run, because a
/// tool that cannot be bounded is not started.
pub fn launcher() -> Result<PathBuf, String> {
    if let Some(named) = std::env::var_os("MOKMOK_CONFINE") {
        return Ok(PathBuf::from(named));
    }
    let exe = std::env::current_exe().map_err(|error| format!("cannot find myself: {error}"))?;
    let beside = exe.with_file_name(LAUNCHER);
    if beside.is_file() {
        return Ok(beside);
    }
    Err(format!(
        "{} is not beside {}: the launcher is what bounds a tool",
        LAUNCHER,
        exe.display()
    ))
}

/// The launcher's name, which is also how it is found beside the daemon.
pub const LAUNCHER: &str = "agent-confine";

/// Runs the bounds on this process and says whether they hold here.
///
/// # Errors
///
/// Returns what the kernel said when a bound cannot be installed, so an operator
/// reads the host's reason rather than a class.
pub fn probe(settings: &Settings) -> Result<(), String> {
    let Some(tool) = settings.tools.iter().find(|tool| tool.confined) else {
        return Ok(());
    };
    let launcher = launcher()?;
    let output = std::process::Command::new(launcher)
        .arg(CHECK)
        .arg(format!("--memory-bytes={}", tool.memory_bytes))
        .arg(format!("--file-bytes={}", tool.file_bytes))
        .arg(format!(
            "--network={}",
            match tool.network {
                Network::None => "none",
                Network::Any => "any",
            }
        ))
        .arg("--")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .output()
        .map_err(|error| format!("cannot start the launcher: {error}"))?;
    if output.status.success() {
        return Ok(());
    }
    Err(format!(
        "the launcher cannot hold a bound on this host: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    ))
}

/// The program a tool's manifest names, found the way the daemon finds programs.
///
/// The child is given only the names its manifest declares, and a program named
/// without a path is found through `PATH`, so the lookup happens here, against the
/// daemon's environment. Which program runs is the manifest's statement about
/// `argv`; what it may see is its statement about `permissions`, and one must not
/// decide the other.
#[must_use]
pub fn resolve(program: &str) -> Option<PathBuf> {
    if program.contains('/') {
        return Some(PathBuf::from(program));
    }
    search(program, &std::env::var_os("PATH")?)
}

fn search(
    program: &str,
    path: &std::ffi::OsStr,
) -> Option<PathBuf> {
    std::env::split_paths(path)
        .map(|directory| directory.join(program))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|meta| {
        meta.is_file()
            && (std::os::unix::fs::PermissionsExt::mode(&meta.permissions()) & 0o111) != 0
    })
}

#[cfg(test)]
mod tests;
