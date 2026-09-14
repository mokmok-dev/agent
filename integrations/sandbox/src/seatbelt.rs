//! Layer-1 confinement for macOS: commands spawn as real processes under a
//! Seatbelt (`sandbox-exec`) profile rendered from the policy.
//!
//! The profile is deny-by-default and confines what this macOS version
//! confines reliably:
//!
//! - **Process execution** is limited to the confined `PATH` directories.
//! - **File writes** are limited to the read-write mounts, `/dev/null`, and
//!   the sandbox scratch directory; the process and every descendant share
//!   the fate of the process group on timeout.
//! - **Network** is denied entirely: `deny default` allows no socket setup.
//! - **File reads are NOT path-confined** at the OS level: on macOS 26,
//!   platform binaries abort inside `dyld4::CacheFinder` when their reads are
//!   filtered (`file-read*`/`file-read-data` with subpath filters abort
//!   reliably, while broad read grants are stable), so the profile grants
//!   reads at `/` and read confinement stays at the [`Vfs`](crate::vfs::Vfs)
//!   layer. This is a stated gap; see `docs/sandbox.md`.
//!
//! Seatbelt is officially unsupported by Apple and profiles are best-effort.

use std::fs;
use std::io;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time::timeout;

use crate::error::SandboxError;
use crate::executor::ExecResult;
use crate::policy::{CommandPrefix, Mount, MountSource, Policy};

/// The directories the confined `PATH` is built from; everything on it is
/// also executable under the profile.
const SYSTEM_BIN_DIRS: &[&str] = &["/bin", "/usr/bin", "/usr/local/bin", "/opt/homebrew/bin"];

/// The Seatbelt front-end. Unconfined itself: it applies the profile to its
/// child.
const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// The shell the command runs under.
const BASH: &str = "/bin/bash";

const EXIT_CANNOT_EXECUTE: i32 = 126;
const EXIT_TIMED_OUT: i32 = 124;
const SIGKILL_EXIT: i32 = 128 + 9;

/// The macOS layer-1 executor: renders a Seatbelt profile from the policy at
/// construction and spawns commands under it.
///
/// The policy is bound at construction, so a running executor cannot widen
/// its own permissions.
pub struct ConfinedProcessExecutor {
    profile_path: PathBuf,
    scratch: PathBuf,
    workdir: PathBuf,
    env: Vec<(String, String)>,
    path_env: String,
    timeout: Duration,
    max_output_bytes: u64,
}

impl Drop for ConfinedProcessExecutor {
    fn drop(&mut self) {
        // Best-effort cleanup; a leaked scratch directory is bounded by /tmp
        // hygiene and holds nothing the host cares about.
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
            .field("workdir", &self.workdir)
            .field("scratch", &self.scratch)
            .finish_non_exhaustive()
    }
}

impl ConfinedProcessExecutor {
    /// Builds the executor from the policy.
    ///
    /// # Errors
    ///
    /// Fails closed with [`SandboxError::UnsupportedPlatform`] when
    /// `sandbox-exec` is unavailable, [`SandboxError::InvalidPolicy`] for an
    /// unusable workdir or allowlist, and [`SandboxError::Io`] when the
    /// scratch directory or profile cannot be written.
    pub fn new(policy: &Policy) -> Result<Self, SandboxError> {
        if fs::metadata(SANDBOX_EXEC).is_err() {
            return Err(SandboxError::UnsupportedPlatform(
                "sandbox-exec is not available",
            ));
        }
        let workdir = validate_workdir(&policy.shell.workdir, &policy.fs.mounts)?;
        let scratch = create_scratch()?;
        let path_dirs = confined_path_dirs(&policy.shell.allow)?;
        let path_env = std::env::join_paths(path_dirs.iter().map(Path::new))
            .map_err(|error| {
                SandboxError::InvalidPolicy(format!("confined PATH is malformed: {error}"))
            })?
            .to_string_lossy()
            .into_owned();

        let profile = render_profile(policy, &scratch, &path_dirs);
        let profile_path = scratch.join("profile.sb");
        fs::write(&profile_path, profile)?;

        let env = policy
            .shell
            .env
            .iter()
            .map(|variable| (variable.name.clone(), variable.value.clone()))
            .collect();
        Ok(Self {
            profile_path,
            scratch,
            workdir,
            env,
            path_env,
            timeout: policy.limits.timeout,
            max_output_bytes: policy.limits.max_output_bytes,
        })
    }

    fn cannot_execute(message: &str) -> ExecResult {
        ExecResult {
            stdout: String::new(),
            stderr: format!("[agentd-sandbox] {message}"),
            exit_code: EXIT_CANNOT_EXECUTE,
            denied_by: None,
        }
    }

    async fn run(
        &self,
        command: &str,
    ) -> ExecResult {
        let mut std_command = std::process::Command::new(SANDBOX_EXEC);
        std_command
            .arg("-f")
            .arg(&self.profile_path)
            .arg(BASH)
            .arg("-c")
            .arg(command);
        std_command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        std_command.env_clear();
        for (name, value) in &self.env {
            std_command.env(name, value);
        }
        std_command.env("PATH", &self.path_env);
        std_command.env("HOME", &self.scratch);
        std_command.env("TMPDIR", &self.scratch);
        std_command.current_dir(&self.workdir);
        // The child leads its own process group so a timeout or output-cap
        // kill takes the whole tree down, not just the shell.
        std_command.process_group(0);

        let mut child = match Command::from(std_command).spawn() {
            Ok(child) => child,
            Err(error) => {
                return Self::cannot_execute(&format!(
                    "spawning the confined command failed: {error}"
                ));
            },
        };
        let pid = child.id();

        let stdout_data = &mut Vec::new();
        let stderr_data = &mut Vec::new();
        let remaining = Arc::new(AtomicU64::new(self.max_output_bytes));
        let truncated = Arc::new(AtomicBool::new(false));
        let stdout_pipe = child.stdout.take();
        let stderr_pipe = child.stderr.take();

        let wait = async {
            let stdout_state = Capture {
                data: stdout_data,
                remaining: remaining.clone(),
                truncated: truncated.clone(),
                pid,
            };
            let stderr_state = Capture {
                data: stderr_data,
                remaining: remaining.clone(),
                truncated: truncated.clone(),
                pid,
            };
            let (stdout_read, stderr_read) = tokio::join!(
                capture(stdout_pipe, stdout_state),
                capture(stderr_pipe, stderr_state),
            );
            match (stdout_read, stderr_read) {
                (Ok(()), Ok(())) => child.wait().await,
                (Err(error), _) | (_, Err(error)) => Err(error),
            }
        };

        let (mut exit_code, timed_out) = match timeout(self.timeout, Box::pin(wait)).await {
            Ok(Ok(status)) => (
                status.code().unwrap_or_else(|| {
                    status
                        .signal()
                        .map_or(EXIT_CANNOT_EXECUTE, |signal| 128 + signal)
                }),
                false,
            ),
            Ok(Err(error)) => {
                kill_group(pid);
                let _ = child.wait().await;
                return Self::cannot_execute(&format!("waiting for the command failed: {error}"));
            },
            Err(_) => {
                kill_group(pid);
                let _ = timeout(Duration::from_secs(5), child.wait()).await;
                (EXIT_TIMED_OUT, true)
            },
        };
        // The capture task already killed the process group when the cap was
        // hit; the child is reaped by now, so the pid must not be signalled
        // again (it may belong to someone else).
        if truncated.load(Ordering::Relaxed) {
            exit_code = exit_code.max(SIGKILL_EXIT);
        }

        let stdout = String::from_utf8_lossy(stdout_data).into_owned();
        let mut stderr = String::from_utf8_lossy(stderr_data).into_owned();
        if timed_out {
            stderr.push_str("\n[agentd-sandbox] command exceeded the wall-clock timeout");
        }
        if truncated.load(Ordering::Relaxed) {
            stderr.push_str("\n[agentd-sandbox] output truncated at max_output_bytes");
        }
        ExecResult {
            stdout,
            stderr,
            exit_code,
            denied_by: None,
        }
    }
}

fn kill_group(pid: Option<u32>) {
    let Some(pid) = pid else {
        return;
    };
    let Ok(raw) = i32::try_from(pid) else {
        return;
    };
    let _ = killpg(Pid::from_raw(raw), Signal::SIGKILL);
}

/// One output-capture pipe's read loop: appends up to the shared remaining
/// byte budget, then kills the process group so the producer cannot fill the
/// pipe forever.
struct Capture<'a> {
    data: &'a mut Vec<u8>,
    remaining: Arc<AtomicU64>,
    truncated: Arc<AtomicBool>,
    pid: Option<u32>,
}

async fn capture<R: tokio::io::AsyncRead + Unpin>(
    pipe: Option<R>,
    state: Capture<'_>,
) -> io::Result<()> {
    let Some(mut pipe) = pipe else {
        return Ok(());
    };
    let mut buffer = [0_u8; 8192];
    loop {
        let read = pipe.read(&mut buffer).await?;
        if read == 0 {
            return Ok(());
        }
        let allowed = usize::try_from(state.remaining.load(Ordering::Relaxed))
            .unwrap_or(usize::MAX)
            .min(read);
        state.data.extend_from_slice(&buffer[..allowed]);
        state.remaining.fetch_sub(allowed as u64, Ordering::Relaxed);
        if allowed < read {
            state.truncated.store(true, Ordering::Relaxed);
            kill_group(state.pid);
            return Ok(());
        }
    }
}

fn validate_workdir(
    workdir: &Path,
    mounts: &[Mount],
) -> Result<PathBuf, SandboxError> {
    if !workdir.is_absolute() {
        return Err(SandboxError::InvalidPolicy(format!(
            "workdir {:?} must be absolute",
            workdir.display().to_string()
        )));
    }
    let metadata = fs::metadata(workdir).map_err(|error| {
        SandboxError::InvalidPolicy(format!(
            "workdir {} is not accessible: {error}",
            workdir.display()
        ))
    })?;
    if !metadata.is_dir() {
        return Err(SandboxError::InvalidPolicy(format!(
            "workdir {} is not a directory",
            workdir.display()
        )));
    }
    let canonical = workdir.canonicalize()?;
    let covered = mounts.iter().any(|mount| match &mount.source {
        MountSource::Mem => false,
        MountSource::ReadOnly { host }
        | MountSource::ReadWrite { host }
        | MountSource::Overlay { host } => host
            .canonicalize()
            .is_ok_and(|host| canonical.starts_with(&host)),
    });
    if !covered {
        return Err(SandboxError::InvalidPolicy(
            "workdir must be inside a host-backed mount so the process and the VFS see the same tree"
                .to_string(),
        ));
    }
    Ok(canonical)
}

fn create_scratch() -> Result<PathBuf, SandboxError> {
    let scratch = std::env::temp_dir().join(format!("agentd-sandbox-{}", uuid::Uuid::now_v7()));
    fs::create_dir_all(&scratch)?;
    Ok(scratch)
}

/// Builds the confined `PATH`: the system directories that exist plus the
/// directories holding allowlisted commands.
fn confined_path_dirs(allow: &[CommandPrefix]) -> Result<Vec<PathBuf>, SandboxError> {
    let mut dirs: Vec<PathBuf> = SYSTEM_BIN_DIRS
        .iter()
        .filter(|dir| Path::new(dir).is_dir())
        .map(PathBuf::from)
        .collect();
    for prefix in allow {
        let Some(tokens) = shlex::split(prefix.as_str()) else {
            return Err(SandboxError::InvalidPolicy(format!(
                "allowlist entry {:?} is not tokenizable",
                prefix.as_str()
            )));
        };
        let Some(first) = tokens.first() else {
            continue;
        };
        if first.contains('/') {
            let path = PathBuf::from(first);
            if !path.is_absolute() {
                return Err(SandboxError::InvalidPolicy(format!(
                    "allowlist entry {first:?} must be an absolute path or a bare command name"
                )));
            }
            if let Some(parent) = path.parent()
                && parent.is_dir()
                && !dirs.contains(&parent.to_path_buf())
            {
                dirs.push(parent.to_path_buf());
            }
        }
    }
    dirs.dedup();
    Ok(dirs)
}

/// The canonical host directories that receive OS-level write grants: the
/// read-write mounts plus the scratch directory (added by the caller).
/// Overlay and read-only mounts are intentionally read-only at the OS level.
fn writable_hosts(policy: &Policy) -> Vec<PathBuf> {
    let mut writable: Vec<PathBuf> = Vec::new();
    for mount in &policy.fs.mounts {
        let MountSource::ReadWrite { host } = &mount.source else {
            continue;
        };
        if let Ok(canonical) = host.canonicalize() {
            writable.push(canonical);
        }
    }
    writable
}

fn subpath_clauses(paths: impl Iterator<Item = PathBuf>) -> Vec<String> {
    let mut clauses: Vec<String> = paths
        .map(|path| format!("(subpath {:?})", path.display().to_string()))
        .collect();
    clauses.sort();
    clauses.dedup();
    clauses
}

fn render_profile(
    policy: &Policy,
    scratch: &Path,
    path_dirs: &[PathBuf],
) -> String {
    let writable = writable_hosts(policy);
    let mut lines = vec![
        String::from("(version 1)"),
        String::from("(deny default)"),
        String::from("(allow process-fork)"),
    ];

    lines.push(format!(
        "(allow process-exec {})",
        subpath_clauses(path_dirs.iter().cloned()).join(" ")
    ));

    // Reads are deliberately broad; see the module docs for the macOS 26
    // dyld abort that rules out filtered read grants.
    lines.push(String::from("(allow file-read-data (subpath \"/\"))"));
    lines.push(String::from("(allow file-read-metadata)"));
    lines.push(String::from("(allow file-read-xattr)"));

    let mut write_clauses = subpath_clauses(
        writable
            .iter()
            .cloned()
            .chain(std::iter::once(scratch.to_path_buf())),
    );
    write_clauses.push(String::from("(literal \"/dev/null\")"));
    lines.push(format!("(allow file-write* {})", write_clauses.join(" ")));

    lines.push(String::from("(allow sysctl-read)"));
    lines.join("\n")
}

#[async_trait::async_trait]
impl crate::executor::Executor for ConfinedProcessExecutor {
    async fn exec(
        &self,
        command: &str,
    ) -> ExecResult {
        Box::pin(self.run(command)).await
    }
}

#[cfg(test)]
mod tests {
    use super::{ConfinedProcessExecutor, confined_path_dirs, render_profile, validate_workdir};
    use crate::error::SandboxError;
    use crate::executor::Executor;
    use crate::policy::{CommandPrefix, EnvVar, FsPolicy, Mount, MountSource, Policy, ShellPolicy};
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};
    use tempfile::TempDir;

    fn overlay_policy(workdir: &Path) -> Policy {
        Policy {
            fs: FsPolicy {
                mounts: vec![Mount {
                    at: PathBuf::from("/work"),
                    source: MountSource::Overlay {
                        host: workdir.to_path_buf(),
                    },
                }],
                ..FsPolicy::default()
            },
            shell: ShellPolicy {
                allow: base_allowlist(),
                workdir: workdir.to_path_buf(),
                ..ShellPolicy::default()
            },
            ..Policy::default()
        }
    }

    fn base_allowlist() -> Vec<CommandPrefix> {
        [
            "echo", "ls", "cat", "touch", "sleep", "yes", "env", "sh", "head",
        ]
        .iter()
        .map(|name| CommandPrefix::new(*name))
        .collect()
    }

    fn executor(policy: &Policy) -> ConfinedProcessExecutor {
        ConfinedProcessExecutor::new(policy).expect("a valid policy builds an executor")
    }

    /// The Nix build sandbox is itself a Seatbelt sandbox and refuses to nest
    /// `sandbox-exec`, so the spawn tests cannot run there. They still run on
    /// developer machines, where the confinement is real.
    fn spawn_tests_supported() -> bool {
        let in_nix_build_sandbox = std::env::var_os("NIX_BUILD_TOP").is_some();
        if in_nix_build_sandbox {
            eprintln!("skipping: sandbox-exec cannot run inside the Nix build sandbox");
        }
        !in_nix_build_sandbox
    }

    #[test]
    fn profile_is_deny_by_default_and_grants_only_mounts() {
        let dir = TempDir::new().expect("tempdir");
        let host = dir.path().canonicalize().expect("canonical tempdir");
        let rw_host = dir
            .path()
            .canonicalize()
            .expect("canonical tempdir")
            .join("rw");
        std::fs::create_dir(&rw_host).expect("rw dir");
        let policy = Policy {
            fs: FsPolicy {
                mounts: vec![
                    Mount {
                        at: PathBuf::from("/work"),
                        source: MountSource::Overlay { host: host.clone() },
                    },
                    Mount {
                        at: PathBuf::from("/data"),
                        source: MountSource::ReadWrite {
                            host: rw_host.clone(),
                        },
                    },
                ],
                ..FsPolicy::default()
            },
            shell: ShellPolicy {
                workdir: host.join("work"),
                ..ShellPolicy::default()
            },
            ..Policy::default()
        };
        // The workdir must exist for validation.
        std::fs::create_dir(host.join("work")).expect("workdir");
        assert!(validate_workdir(&policy.shell.workdir, &policy.fs.mounts).is_ok());

        let profile = render_profile(&policy, Path::new("/tmp/scratch"), &[PathBuf::from("/bin")]);

        assert!(profile.starts_with("(version 1)\n(deny default)"));
        assert!(profile.contains("(allow process-fork)"));
        assert!(profile.contains("(subpath \"/bin\")"));
        assert!(profile.contains("(allow file-read-metadata)"));
        assert!(profile.contains("(allow sysctl-read)"));
        // Reads are deliberately broad (the macOS 26 dyld gap); writes stay
        // confined to the read-write mount, the scratch directory, and
        // /dev/null.
        assert!(profile.contains("(allow file-read-data (subpath \"/\"))"));
        let write_line = profile
            .lines()
            .find(|line| line.starts_with("(allow file-write*"))
            .expect("a write grant");
        assert!(
            write_line.contains(&format!("(subpath {:?})", rw_host.display().to_string())),
            "the read-write mount must be writable: {write_line}"
        );
        assert!(
            !write_line.contains(&format!("(subpath {:?})", host.display().to_string())),
            "the overlay host must not be writable: {write_line}"
        );
        assert!(write_line.contains("(literal \"/dev/null\")"));
    }

    #[test]
    fn workdir_validation_fails_closed() {
        let dir = TempDir::new().expect("tempdir");
        let host = dir.path().canonicalize().expect("canonical tempdir");
        let mounts = vec![Mount {
            at: PathBuf::from("/work"),
            source: MountSource::Overlay { host: host.clone() },
        }];

        let relative = Policy {
            shell: ShellPolicy {
                workdir: PathBuf::from("relative"),
                ..ShellPolicy::default()
            },
            ..Policy::default()
        };
        assert!(matches!(
            validate_workdir(&relative.shell.workdir, &mounts),
            Err(SandboxError::InvalidPolicy(_))
        ));

        let missing = Policy {
            shell: ShellPolicy {
                workdir: host.join("missing"),
                ..ShellPolicy::default()
            },
            ..Policy::default()
        };
        assert!(matches!(
            validate_workdir(&missing.shell.workdir, &mounts),
            Err(SandboxError::InvalidPolicy(_))
        ));

        // A workdir no host mount covers breaks the single-plane guarantee.
        let other = TempDir::new().expect("tempdir");
        let uncovered = other.path().canonicalize().expect("canonical tempdir");
        std::fs::create_dir(uncovered.join("elsewhere")).expect("dir");
        let elsewhere = Policy {
            shell: ShellPolicy {
                workdir: uncovered.join("elsewhere"),
                ..ShellPolicy::default()
            },
            ..Policy::default()
        };
        assert!(matches!(
            validate_workdir(&elsewhere.shell.workdir, &mounts),
            Err(SandboxError::InvalidPolicy(_))
        ));
    }

    #[test]
    fn confined_path_dirs_reject_unusable_entries() {
        let ok = vec![CommandPrefix::new("echo"), CommandPrefix::new("ls")];
        assert!(
            !confined_path_dirs(&ok)
                .expect("bare commands resolve from system dirs")
                .is_empty()
        );

        let unterminated = vec![CommandPrefix::new("echo 'unterminated")];
        assert!(matches!(
            confined_path_dirs(&unterminated),
            Err(SandboxError::InvalidPolicy(_))
        ));

        let relative = vec![CommandPrefix::new("tools/bin/echo")];
        assert!(matches!(
            confined_path_dirs(&relative),
            Err(SandboxError::InvalidPolicy(_))
        ));
    }

    #[test]
    fn executor_requires_sandbox_exec_and_writes_the_profile() {
        let dir = TempDir::new().expect("tempdir");
        let host = dir.path().canonicalize().expect("canonical tempdir");
        let policy = overlay_policy(&host);

        let executor = executor(&policy);
        assert!(executor.profile_path.exists());
        let profile =
            std::fs::read_to_string(&executor.profile_path).expect("profile written to scratch");
        assert!(profile.contains("(deny default)"));
        assert!(profile.contains("(subpath \"/bin\")"));
    }

    #[test]
    fn exec_echoes_and_confines_env() {
        if !spawn_tests_supported() {
            return;
        }
        let dir = TempDir::new().expect("tempdir");
        let host = dir.path().canonicalize().expect("canonical tempdir");
        let mut policy = overlay_policy(&host);
        policy.shell.env = vec![EnvVar {
            name: String::from("SANDBOX_MARKER"),
            value: String::from("present"),
        }];
        let executor = executor(&policy);

        let result = executor.blocking_exec("echo hello");

        assert_eq!(result.exit_code, 0, "stderr: {}", result.stderr);
        assert_eq!(result.stdout, "hello\n");
        assert!(!result.is_denied());
    }

    #[test]
    fn writes_reach_read_write_mounts_but_not_overlay_hosts() {
        if !spawn_tests_supported() {
            return;
        }
        let dir = TempDir::new().expect("tempdir");
        let host = dir.path().canonicalize().expect("canonical tempdir");
        let rw_host = host.join("rw");
        std::fs::create_dir(&rw_host).expect("rw dir");
        let policy = Policy {
            fs: FsPolicy {
                mounts: vec![
                    Mount {
                        at: PathBuf::from("/work"),
                        source: MountSource::Overlay { host: host.clone() },
                    },
                    Mount {
                        at: PathBuf::from("/data"),
                        source: MountSource::ReadWrite {
                            host: rw_host.clone(),
                        },
                    },
                ],
                ..FsPolicy::default()
            },
            shell: ShellPolicy {
                allow: base_allowlist(),
                workdir: rw_host.clone(),
                ..ShellPolicy::default()
            },
            ..Policy::default()
        };
        // The process sees host paths: the workdir inside the mount is the
        // rendezvous point between the two planes.
        let rw_policy = Policy {
            shell: ShellPolicy {
                allow: base_allowlist(),
                workdir: rw_host.clone(),
                ..ShellPolicy::default()
            },
            fs: policy.fs.clone(),
            ..policy
        };
        let rw_executor = executor(&rw_policy);
        let write_rw = rw_executor.blocking_exec("echo data > out.txt");
        assert_eq!(write_rw.exit_code, 0, "stderr: {}", write_rw.stderr);
        assert!(rw_host.join("out.txt").exists());

        // The same host directory behind an overlay mount is read-only at
        // the OS level: writes stay in the VFS upper layer, never on disk.
        let overlay_policy_here = overlay_policy(&host);
        let overlay_executor = executor(&overlay_policy_here);
        let write_overlay = overlay_executor.blocking_exec("touch blocked.txt");
        assert_ne!(
            write_overlay.exit_code, 0,
            "overlay host must be read-only at the OS level: stderr {}",
            write_overlay.stderr
        );
        assert!(
            !overlay_executor.workdir.join("blocked.txt").exists(),
            "the overlay host directory must stay untouched"
        );
    }

    #[test]
    fn writes_outside_mounts_are_denied() {
        if !spawn_tests_supported() {
            return;
        }
        let dir = TempDir::new().expect("tempdir");
        let host = dir.path().canonicalize().expect("canonical tempdir");
        let executor = executor(&overlay_policy(&host));

        // Reads are deliberately broad (the module docs' macOS 26 dyld gap);
        // what the OS must enforce is the write boundary.
        let etc = executor.blocking_exec("touch /etc/agentd-sandbox-blocked");
        assert_ne!(etc.exit_code, 0, "stderr: {}", etc.stderr);
        assert!(!Path::new("/etc/agentd-sandbox-blocked").exists());

        let tmp = executor.blocking_exec("touch /tmp/agentd-sandbox-blocked");
        assert_ne!(tmp.exit_code, 0, "stderr: {}", tmp.stderr);
        assert!(!Path::new("/tmp/agentd-sandbox-blocked").exists());
    }

    #[test]
    fn timeout_kills_the_process_group_quickly() {
        if !spawn_tests_supported() {
            return;
        }
        let dir = TempDir::new().expect("tempdir");
        let host = dir.path().canonicalize().expect("canonical tempdir");
        let mut policy = overlay_policy(&host);
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
    fn output_flood_is_truncated_and_killed() {
        if !spawn_tests_supported() {
            return;
        }
        let dir = TempDir::new().expect("tempdir");
        let host = dir.path().canonicalize().expect("canonical tempdir");
        let mut policy = overlay_policy(&host);
        policy.limits.max_output_bytes = 100_000;
        let executor = executor(&policy);

        let started = Instant::now();
        let result = executor.blocking_exec("yes");

        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(
            result.stdout.len() < 200_000,
            "captured {} bytes, expected the cap to hold",
            result.stdout.len()
        );
        assert!(result.stderr.contains("truncated"));
        assert_eq!(result.exit_code, 128 + 9);
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
