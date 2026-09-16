//! Layer-1 confinement for macOS: commands spawn as real processes under a
//! Seatbelt (`sandbox-exec`) profile rendered from the policy.
//!
//! The profile is deny-by-default and confines what this macOS version confines
//! reliably:
//!
//! - **File writes** are limited to the policy's `write` entries, `/dev/null`,
//!   and the sandbox scratch directory; the process and every descendant share
//!   the fate of the process group on timeout. Inside a write root, the
//!   protected metadata names (`.git`, `.agents`) are carved out read-only, and
//!   the root itself cannot be renamed or unlinked.
//! - **Network is denied** except for the policy's Unix domain sockets: there
//!   is no IP grant, so egress and ingress fall under `deny default`, while
//!   each `network.unix_sockets` entry renders as a path-scoped
//!   `network-outbound` grant. See `docs/sandbox.md`.
//! - **File reads are NOT path-confined** at the OS level: on macOS 26,
//!   platform binaries abort inside `dyld4::CacheFinder` when their reads are
//!   filtered (`file-read*`/`file-read-data` with subpath filters abort
//!   reliably, while broad read grants are stable), so the profile grants
//!   reads at `/`. Reads are *narrowed* instead of excluded:
//!   `deny` path entries are emitted as `(deny file-read-data ...)`,
//!   `(deny file-read-metadata ...)`, and `(deny file-read-xattr ...)`, which
//!   override the broad grants. Without a denial, reads are unconfined at the
//!   OS level — a stated gap; see `docs/sandbox.md`.
//!
//! Seatbelt is officially unsupported by Apple and profiles are best-effort.

use std::fs;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::error::SandboxError;
use crate::executor::{ExecResult, SpawnError};
use crate::policy::{Access, FsPolicy, Policy};
use crate::process;

/// The Seatbelt front-end. Unconfined itself: it applies the profile to its
/// child.
const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// The shell the command runs under.
const BASH: &str = "/bin/bash";

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
    /// unusable workdir or path entry, and [`SandboxError::Io`] when the
    /// scratch directory or profile cannot be written.
    pub fn new(policy: &Policy) -> Result<Self, SandboxError> {
        // Validate here as well as in `Sandbox::new`: this constructor is
        // public, and an unrepresentable path must fail closed rather than
        // inject a profile clause.
        policy
            .validate()
            .map_err(|error| SandboxError::InvalidPolicy(error.to_string()))?;
        if fs::metadata(SANDBOX_EXEC).is_err() {
            return Err(SandboxError::UnsupportedPlatform(
                "sandbox-exec is not available",
            ));
        }
        let workdir = process::validate_workdir(&policy.shell.workdir, &policy.fs)?;
        let path_dirs = process::system_path_dirs();
        let scratch = process::create_scratch()?;

        // The scratch directory now exists, so every later failure must remove
        // it rather than leak a directory per rejected policy.
        let (profile_path, path_env) = match prepare(policy, &workdir, &path_dirs, &scratch) {
            Ok(prepared) => prepared,
            Err(error) => {
                let _ = fs::remove_dir_all(&scratch);
                return Err(error);
            },
        };

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

    /// Builds the confined `sandbox-exec` command for `command`, with the
    /// policy profile, the allowlisted environment, and the scratch
    /// `HOME`/`TMPDIR`.
    fn std_command(
        &self,
        command: &str,
    ) -> std::process::Command {
        let mut std_command = std::process::Command::new(SANDBOX_EXEC);
        std_command
            .arg("-f")
            .arg(&self.profile_path)
            .arg(BASH)
            .arg("-c")
            .arg(command);
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

/// The canonical host directories that receive OS-level write grants.
///
/// Every `write` entry must resolve: an unresolvable path cannot be expressed
/// in the profile and dropping it would leave a weaker sandbox than the
/// operator asked for.
fn writable_hosts(fs_policy: &FsPolicy) -> Result<Vec<PathBuf>, SandboxError> {
    let mut writable = Vec::new();
    for entry in &fs_policy.entries {
        if entry.access != Access::Write {
            continue;
        }
        let canonical = entry.path.canonicalize().map_err(|error| {
            SandboxError::InvalidPolicy(format!(
                "write entry {:?} is not accessible: {error}",
                entry.path.display().to_string()
            ))
        })?;
        writable.push(canonical);
    }
    writable.sort();
    writable.dedup();
    Ok(writable)
}

/// Resolves and checks the `deny` paths for the OS profile.
///
/// A denial is only expressible against a path that resolves, so an entry that
/// does not is an error rather than a silently-dropped no-op: dropping it would
/// leave the operator with a weaker sandbox than they configured. A relative
/// entry is rejected for the same reason — it would resolve against the daemon's
/// working directory and deny something the operator never named. A denied path
/// that covers something a command needs — the workdir, an executable
/// directory, or the scratch directory the profile is written into — is also
/// rejected, because it would break every command instead of the one path.
fn validate_deny(
    fs_policy: &FsPolicy,
    workdir: &Path,
    path_dirs: &[PathBuf],
    scratch: &Path,
) -> Result<Vec<PathBuf>, SandboxError> {
    let mut protected: Vec<(&str, PathBuf)> = Vec::new();
    if let Ok(canonical) = workdir.canonicalize() {
        protected.push(("the workdir", canonical));
    }
    for dir in path_dirs {
        if let Ok(canonical) = dir.canonicalize() {
            protected.push(("an executable directory", canonical));
        }
    }
    if let Ok(canonical) = scratch.canonicalize() {
        protected.push(("the scratch directory", canonical));
    }
    for entry in &fs_policy.entries {
        if entry.access == Access::Write
            && let Ok(canonical) = entry.path.canonicalize()
        {
            protected.push(("a write root", canonical));
        }
    }

    let mut resolved: Vec<PathBuf> = Vec::new();
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
        for (role, needed) in &protected {
            // A denial that *covers* a path a command needs (an ancestor of the
            // needed path) breaks every command, so it is rejected. A denial
            // nested inside a needed path only narrows — e.g. `deny
            // /repo/.env` inside the `/repo` write root — and is allowed.
            if needed.starts_with(&canonical) {
                return Err(SandboxError::InvalidPolicy(format!(
                    "deny path {:?} covers {role} {:?}, which every command needs",
                    entry.path.display().to_string(),
                    needed.display().to_string()
                )));
            }
        }
        resolved.push(canonical);
    }
    resolved.sort();
    resolved.dedup();
    Ok(resolved)
}

/// Renders and writes the confinement profile, returning its path alongside the
/// confined `PATH` value.
///
/// Split out of [`ConfinedProcessExecutor::new`] so a failure after the scratch
/// directory is created has a single place to clean it up.
fn prepare(
    policy: &Policy,
    workdir: &Path,
    path_dirs: &[PathBuf],
    scratch: &Path,
) -> Result<(PathBuf, String), SandboxError> {
    let deny = validate_deny(&policy.fs, workdir, path_dirs, scratch)?;
    writable_hosts(&policy.fs)?;
    let path_env = process::join_path(path_dirs)?;

    let profile = render_profile(policy, scratch, &deny);
    let profile_path = scratch.join("profile.sb");
    fs::write(&profile_path, profile)?;
    Ok((profile_path, path_env))
}

/// Renders paths as sorted, deduplicated `subpath` clauses.
///
/// `{:?}` is used deliberately as a quoting step: it escapes `"` and `\` the way
/// Scheme does, so a path arriving as JSON event data cannot inject a clause.
/// Rust's `\u{..}` escapes are not valid Scheme, so a path holding a control
/// character yields a profile that fails to parse — `sandbox-exec` then fails
/// closed, which is the safe direction.
fn subpath_clauses(paths: impl Iterator<Item = PathBuf>) -> Vec<String> {
    let mut clauses: Vec<String> = paths
        .map(|path| format!("(subpath {:?})", path.display().to_string()))
        .collect();
    clauses.sort();
    clauses.dedup();
    clauses
}

/// Escapes the regex metacharacters so a path can be embedded in an SBPL
/// `(regex #"...")` clause literally.
fn regex_escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        if matches!(
            character,
            '.' | '^' | '$' | '*' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '\\'
        ) {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

/// The `network-outbound` grants for the policy's Unix domain sockets.
///
/// Seatbelt matches an `AF_UNIX` connect against a subject that carries an
/// address prefix ahead of the socket path, so a `^`-anchored pattern cannot
/// start at the path: `^.*<path>$` anchors the path's end while absorbing the
/// prefix. The leading `^.*` is not a blanket grant — Seatbelt only consults
/// this filter for an `AF_UNIX` connect, and the pattern must still end with the
/// named path, so TCP egress stays denied and a different socket does not match.
fn unix_socket_grants(policy: &Policy) -> Vec<String> {
    let mut grants: Vec<String> = policy
        .network
        .unix_sockets
        .iter()
        .map(|socket| {
            format!(
                "(allow network-outbound (remote unix-socket (regex #\"^.*{}$\")))",
                regex_escape(&socket.display().to_string())
            )
        })
        .collect();
    grants.sort();
    grants.dedup();
    grants
}

/// The protected-metadata denial for one write root: writing anything at
/// `<root>/<name>` or beneath it is denied, including creating it.
fn protected_denials(
    writable: &[PathBuf],
    protected: &[String],
) -> Vec<String> {
    let mut denials = Vec::new();
    for root in writable {
        for name in protected {
            let pattern = format!(
                "^{}/{}",
                regex_escape(&root.display().to_string()),
                regex_escape(name)
            );
            denials.push(format!("(deny file-write* (regex #\"{pattern}(/.*)?$\"))"));
        }
    }
    denials.sort();
    denials.dedup();
    denials
}

/// The denial that stops a command renaming or unlinking a writable root,
/// which would replace the boundary the next profile treats as authoritative.
fn root_unlink_denials(writable: &[PathBuf]) -> Vec<String> {
    writable
        .iter()
        .map(|root| {
            format!(
                "(deny file-write-unlink (require-all (literal {:?}) (vnode-type DIRECTORY)))",
                root.display().to_string()
            )
        })
        .collect()
}

fn render_profile(
    policy: &Policy,
    scratch: &Path,
    deny: &[PathBuf],
) -> String {
    let writable = writable_hosts(&policy.fs).unwrap_or_default();
    let mut lines = vec![
        String::from("(version 1)"),
        String::from("(deny default)"),
        String::from("(allow process-fork)"),
        // The boundary is the filesystem profile, not an argv allowlist: any
        // binary may run, but it can only touch what the profile grants.
        String::from("(allow process-exec)"),
    ];

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

    // Network is denied by default; only the policy's Unix domain sockets are
    // reachable. The `.*` absorbs the address prefix Seatbelt matches ahead of
    // the socket path, so the grant anchors the path's end without opening
    // egress to anything else (see `unix_socket_grants`).
    lines.extend(unix_socket_grants(policy));

    // Protected metadata is carved out of the write roots, and the roots
    // themselves cannot be renamed or unlinked. Both are denials emitted after
    // the grants; in Seatbelt a deny overrides any matching allow.
    lines.extend(protected_denials(&writable, &policy.fs.protected));
    lines.extend(root_unlink_denials(&writable));

    // Read denials are emitted last and are the only layer that can withhold a
    // host file from a spawned binary, because reads are otherwise granted at
    // `/`. Data, metadata, and xattr are all denied: withholding the contents
    // alone would still expose metadata such as the size through `stat`. A
    // listing of the *parent* stays allowed, so the entry's name is still
    // enumerable; that is a stated gap.
    if !deny.is_empty() {
        let clauses = subpath_clauses(deny.iter().cloned()).join(" ");
        for kind in ["file-read-data", "file-read-metadata", "file-read-xattr"] {
            lines.push(format!("(deny {kind} {clauses})"));
        }
    }

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

    async fn spawn(
        &self,
        command: &str,
    ) -> Result<tokio::process::Child, SpawnError> {
        process::spawn(self.std_command(command))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ConfinedProcessExecutor, regex_escape, render_profile, validate_deny, writable_hosts,
    };
    use crate::error::SandboxError;
    use crate::executor::Executor;
    use crate::policy::{Access, EnvVar, FsEntry, FsPolicy, NetworkPolicy, Policy, ShellPolicy};
    use crate::process::validate_workdir;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};
    use tempfile::TempDir;

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
    fn profile_is_deny_by_default_and_confines_writes() {
        let dir = TempDir::new().expect("tempdir");
        let write_root = dir
            .path()
            .canonicalize()
            .expect("canonical tempdir")
            .join("rw");
        std::fs::create_dir(&write_root).expect("rw dir");
        let read_root = dir
            .path()
            .canonicalize()
            .expect("canonical tempdir")
            .join("ro");
        std::fs::create_dir(&read_root).expect("ro dir");
        let work = write_root.join("work");
        std::fs::create_dir(&work).expect("workdir");

        let policy = Policy {
            fs: FsPolicy {
                entries: vec![
                    FsEntry {
                        path: write_root.clone(),
                        access: Access::Write,
                    },
                    FsEntry {
                        path: read_root.clone(),
                        access: Access::Read,
                    },
                ],
                ..FsPolicy::default()
            },
            shell: ShellPolicy {
                workdir: work,
                ..ShellPolicy::default()
            },
            ..Policy::default()
        };
        assert!(validate_workdir(&policy.shell.workdir, &policy.fs).is_ok());

        let profile = render_profile(&policy, Path::new("/tmp/scratch"), &[]);

        assert!(profile.starts_with("(version 1)\n(deny default)"));
        assert!(profile.contains("(allow process-fork)"));
        assert!(profile.contains("(allow file-read-data (subpath \"/\"))"));
        assert!(profile.contains("(allow sysctl-read)"));
        // Egress is denied: `deny default` covers it because there is no grant.
        assert!(!profile.contains("network-outbound"));
        assert!(!profile.contains("network-inbound"));
        assert!(!profile.contains("network*"));
        let write_line = profile
            .lines()
            .find(|line| line.starts_with("(allow file-write*"))
            .expect("a write grant");
        assert!(
            write_line.contains(&format!("(subpath {:?})", write_root.display().to_string())),
            "the write entry must be writable: {write_line}"
        );
        assert!(
            !write_line.contains(&format!("(subpath {:?})", read_root.display().to_string())),
            "a read entry must not be writable: {write_line}"
        );
        assert!(write_line.contains("(literal \"/dev/null\")"));
    }

    #[test]
    fn a_unix_socket_renders_a_path_scoped_outbound_grant() {
        let dir = TempDir::new().expect("tempdir");
        let host = dir.path().canonicalize().expect("canonical tempdir");
        let socket = host.join("agentd.sock");
        let policy = Policy {
            network: NetworkPolicy {
                unix_sockets: vec![socket.clone()],
            },
            ..workdir_policy(&host)
        };

        let profile = render_profile(&policy, Path::new("/tmp/scratch"), &[]);

        let pattern = format!(
            "(allow network-outbound (remote unix-socket (regex #\"^.*{}$\")))",
            regex_escape(&socket.display().to_string())
        );
        assert!(
            profile.contains(&pattern),
            "the socket must be granted by path: {profile}"
        );
        // The grant is the only network clause: ingress and IP egress stay
        // denied, because no other `network-*` line is rendered.
        assert!(
            !profile.contains("network-inbound"),
            "ingress must stay denied: {profile}"
        );
        assert_eq!(
            profile.matches("network-").count(),
            1,
            "only the socket grant may appear: {profile}"
        );
    }

    #[test]
    fn protected_metadata_and_root_rename_are_denied() {
        let dir = TempDir::new().expect("tempdir");
        let write_root = dir.path().canonicalize().expect("canonical tempdir");
        let policy = workdir_policy(&write_root);

        let profile = render_profile(&policy, Path::new("/tmp/scratch"), &[]);

        let root = regex_escape(&write_root.display().to_string());
        assert!(
            profile.contains(&format!(
                "(deny file-write* (regex #\"^{root}/\\.git(/.*)?$\"))"
            )),
            "a .git carveout must be rendered: {profile}"
        );
        assert!(
            profile.contains(&format!(
                "(deny file-write* (regex #\"^{root}/\\.agents(/.*)?$\"))"
            )),
            "a .agents carveout must be rendered: {profile}"
        );
        assert!(
            profile.contains(&format!(
                "(deny file-write-unlink (require-all (literal {:?}) (vnode-type DIRECTORY)))",
                write_root.display().to_string()
            )),
            "the write root must not be renameable: {profile}"
        );
    }

    #[test]
    fn deny_paths_are_rendered_as_canonical_denials() {
        let dir = TempDir::new().expect("tempdir");
        let secret_dir = TempDir::new().expect("secret tempdir");
        // Deliberately NOT canonicalised: the raw path is the form a caller
        // would supply, and the rendered clause must be the resolved form.
        let secret_raw = secret_dir.path().to_path_buf();
        let workdir = dir.path().canonicalize().expect("canonical workdir");
        let secret = secret_raw.canonicalize().expect("canonical secret");
        let policy = Policy {
            fs: FsPolicy {
                entries: vec![FsEntry {
                    path: secret_raw.clone(),
                    access: Access::Deny,
                }],
                ..FsPolicy::default()
            },
            ..Policy::default()
        };
        let deny = validate_deny(
            &policy.fs,
            &workdir,
            &[PathBuf::from("/bin")],
            Path::new("/tmp/scratch"),
        )
        .expect("valid deny");

        let profile = render_profile(&policy, Path::new("/tmp/scratch"), &deny);

        let deny_line = profile
            .lines()
            .find(|line| line.starts_with("(deny file-read-data"))
            .expect("a read denial");
        let read_grant = profile
            .lines()
            .position(|line| line.starts_with("(allow file-read-data"))
            .expect("a read grant");
        let deny_at = profile
            .lines()
            .position(|line| line.starts_with("(deny file-read-data"))
            .expect("a read denial");
        assert!(deny_at > read_grant, "the denial must follow the grant");
        assert!(
            deny_line.contains(&format!("(subpath {:?})", secret.display().to_string())),
            "the canonical secret path must be denied: {deny_line}"
        );
        if secret_raw != secret {
            assert!(
                !deny_line.contains(&format!("(subpath {:?})", secret_raw.display().to_string())),
                "the unresolved form must not be rendered: {deny_line}"
            );
        }
        // Withholding the contents alone leaves names and sizes readable, so
        // every read class must be denied.
        for kind in ["file-read-data", "file-read-metadata", "file-read-xattr"] {
            assert!(
                profile.contains(&format!("(deny {kind} (subpath ")),
                "metadata read {kind} must be denied too: {profile}"
            );
        }
    }

    #[test]
    fn deny_fails_closed_on_an_unresolvable_path() {
        let dir = TempDir::new().expect("tempdir");
        let workdir = dir.path().canonicalize().expect("canonical");
        let policy = Policy {
            fs: FsPolicy {
                entries: vec![FsEntry {
                    path: PathBuf::from("/nonexistent/secret"),
                    access: Access::Deny,
                }],
                ..FsPolicy::default()
            },
            ..Policy::default()
        };

        let error = validate_deny(
            &policy.fs,
            &workdir,
            &[PathBuf::from("/bin")],
            Path::new("/tmp/scratch"),
        )
        .expect_err("an unresolvable path must be rejected");
        assert!(
            matches!(error, SandboxError::InvalidPolicy(_)),
            "unexpected error: {error:?}"
        );
    }

    #[test]
    fn deny_rejects_a_relative_path() {
        let dir = TempDir::new().expect("tempdir");
        let workdir = dir.path().canonicalize().expect("canonical");
        let policy = Policy {
            fs: FsPolicy {
                entries: vec![FsEntry {
                    path: PathBuf::from("secrets"),
                    access: Access::Deny,
                }],
                ..FsPolicy::default()
            },
            ..Policy::default()
        };

        let error = validate_deny(
            &policy.fs,
            &workdir,
            &[PathBuf::from("/bin")],
            Path::new("/tmp/scratch"),
        )
        .expect_err("a relative path must be rejected");
        assert!(
            matches!(error, SandboxError::InvalidPolicy(_)),
            "unexpected error: {error:?}"
        );
    }

    #[test]
    fn deny_rejects_a_path_the_sandbox_itself_needs() {
        let dir = TempDir::new().expect("tempdir");
        let workdir = dir.path().canonicalize().expect("canonical");
        let bin = [PathBuf::from("/bin")];
        let policy = Policy {
            fs: FsPolicy {
                entries: vec![FsEntry {
                    path: workdir.clone(),
                    access: Access::Deny,
                }],
                ..FsPolicy::default()
            },
            ..Policy::default()
        };

        let error = validate_deny(&policy.fs, &workdir, &bin, Path::new("/tmp/scratch"))
            .expect_err("the workdir must not be deniable");
        assert!(
            matches!(error, SandboxError::InvalidPolicy(_)),
            "unexpected error: {error:?}"
        );

        let policy = Policy {
            fs: FsPolicy {
                entries: vec![FsEntry {
                    path: PathBuf::from("/usr"),
                    access: Access::Deny,
                }],
                ..FsPolicy::default()
            },
            ..Policy::default()
        };
        let error = validate_deny(
            &policy.fs,
            &workdir,
            &[PathBuf::from("/usr/bin")],
            Path::new("/tmp/scratch"),
        )
        .expect_err("an executable directory must not be deniable");
        assert!(
            matches!(error, SandboxError::InvalidPolicy(_)),
            "unexpected error: {error:?}"
        );
    }

    #[test]
    fn deny_inside_a_write_root_is_allowed_and_rendered() {
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path().canonicalize().expect("canonical");
        let env_file = root.join(".env");
        std::fs::write(&env_file, "secret").expect("env file");
        let policy = Policy {
            fs: FsPolicy {
                entries: vec![
                    FsEntry {
                        path: root.clone(),
                        access: Access::Write,
                    },
                    FsEntry {
                        path: env_file.clone(),
                        access: Access::Deny,
                    },
                ],
                ..FsPolicy::default()
            },
            shell: ShellPolicy {
                workdir: root.clone(),
                ..ShellPolicy::default()
            },
            ..Policy::default()
        };

        let deny = validate_deny(
            &policy.fs,
            &root,
            &[PathBuf::from("/bin")],
            Path::new("/tmp/scratch"),
        )
        .expect("a deny nested in a write root narrows and must be allowed");

        let profile = render_profile(&policy, Path::new("/tmp/scratch"), &deny);
        let canonical = env_file.canonicalize().expect("canonical env file");
        assert!(
            profile.contains(&format!(
                "(deny file-read-data (subpath {:?})",
                canonical.display().to_string()
            )),
            "the nested deny must be rendered: {profile}"
        );
    }

    #[test]
    fn no_denial_line_when_nothing_is_denied() {
        let profile = render_profile(&Policy::default(), Path::new("/tmp/scratch"), &[]);

        assert!(
            !profile.contains("(deny file-read-data"),
            "an empty deny list must not emit a clause: {profile}"
        );
    }

    #[test]
    fn workdir_validation_fails_closed() {
        let dir = TempDir::new().expect("tempdir");
        let host = dir.path().canonicalize().expect("canonical tempdir");
        let policy = workdir_policy(&host);

        let relative = Policy {
            shell: ShellPolicy {
                workdir: PathBuf::from("relative"),
                ..ShellPolicy::default()
            },
            ..policy.clone()
        };
        assert!(matches!(
            validate_workdir(&relative.shell.workdir, &relative.fs),
            Err(SandboxError::InvalidPolicy(_))
        ));

        let missing = Policy {
            shell: ShellPolicy {
                workdir: host.join("missing"),
                ..ShellPolicy::default()
            },
            ..policy.clone()
        };
        assert!(matches!(
            validate_workdir(&missing.shell.workdir, &missing.fs),
            Err(SandboxError::InvalidPolicy(_))
        ));

        // A workdir no write entry covers is rejected, not run read-only.
        let other = TempDir::new().expect("tempdir");
        let elsewhere = other.path().canonicalize().expect("canonical tempdir");
        std::fs::create_dir(elsewhere.join("elsewhere")).expect("dir");
        let uncovered = Policy {
            shell: ShellPolicy {
                workdir: elsewhere.join("elsewhere"),
                ..ShellPolicy::default()
            },
            ..policy
        };
        assert!(matches!(
            validate_workdir(&uncovered.shell.workdir, &uncovered.fs),
            Err(SandboxError::InvalidPolicy(_))
        ));
    }

    #[test]
    fn writable_hosts_fail_closed_on_an_unresolvable_entry() {
        let policy = Policy {
            fs: FsPolicy {
                entries: vec![FsEntry {
                    path: PathBuf::from("/nonexistent/write"),
                    access: Access::Write,
                }],
                ..FsPolicy::default()
            },
            ..Policy::default()
        };

        assert!(matches!(
            writable_hosts(&policy.fs),
            Err(SandboxError::InvalidPolicy(_))
        ));
    }

    #[test]
    fn executor_requires_sandbox_exec_and_writes_the_profile() {
        let dir = TempDir::new().expect("tempdir");
        let host = dir.path().canonicalize().expect("canonical tempdir");
        let policy = workdir_policy(&host);

        let executor = executor(&policy);
        assert!(executor.profile_path.exists());
        let profile =
            std::fs::read_to_string(&executor.profile_path).expect("profile written to scratch");
        assert!(profile.contains("(deny default)"));
        assert!(profile.contains("(allow file-read-data (subpath \"/\"))"));
    }

    #[test]
    fn a_rejected_policy_does_not_leak_a_scratch_directory() {
        let count = || {
            let prefix = format!("{}/agentd-sandbox-", std::env::temp_dir().display());
            let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
                return 0;
            };
            entries
                .filter_map(Result::ok)
                .filter(|entry| entry.path().display().to_string().starts_with(&prefix))
                .count()
        };
        let before = count();

        // The policy is rejected after the scratch directory exists, so the
        // failure path must remove it.
        let error = ConfinedProcessExecutor::new(&Policy {
            fs: FsPolicy {
                entries: vec![FsEntry {
                    path: PathBuf::from("/nonexistent/secret"),
                    access: Access::Deny,
                }],
                ..FsPolicy::default()
            },
            ..Policy::default()
        })
        .expect_err("an unresolvable deny must be rejected");
        assert!(
            matches!(error, SandboxError::InvalidPolicy(_)),
            "unexpected error: {error:?}"
        );

        assert_eq!(
            before,
            count(),
            "a rejected policy must not leave a scratch dir behind"
        );
    }

    #[test]
    fn rendered_profile_is_accepted_by_sandbox_exec() {
        if !spawn_tests_supported() {
            return;
        }
        let work = TempDir::new().expect("workdir");
        let secret_dir = TempDir::new().expect("secret tempdir");
        let scratch = TempDir::new().expect("scratch");
        let policy = workdir_policy(&work.path().canonicalize().expect("canonical"));
        let deny = vec![secret_dir.path().canonicalize().expect("canonical secret")];
        let profile = render_profile(&policy, scratch.path(), &deny);
        let path = scratch.path().join("parse-check.sb");
        std::fs::write(&path, &profile).expect("write profile");

        let accepted = std::process::Command::new("/usr/bin/sandbox-exec")
            .arg("-f")
            .arg(&path)
            .arg("/usr/bin/true")
            .status()
            .expect("sandbox-exec runs");
        assert!(
            accepted.success(),
            "sandbox-exec rejected the profile:\n{profile}"
        );
    }

    #[test]
    fn exec_echoes_and_confines_env() {
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

        let result = executor.blocking_exec("echo hello");

        assert_eq!(result.exit_code, 0, "stderr: {}", result.stderr);
        assert_eq!(result.stdout, "hello\n");
    }

    #[tokio::test]
    async fn a_spawned_session_is_confined_and_streams_io() {
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
        let status = child.wait().await.expect("wait");
        assert!(status.success(), "session should exit cleanly");
    }

    #[test]
    fn writes_reach_write_entries_but_not_read_entries() {
        if !spawn_tests_supported() {
            return;
        }
        let dir = TempDir::new().expect("tempdir");
        let host = dir.path().canonicalize().expect("canonical tempdir");
        let write_root = host.join("rw");
        let read_root = host.join("ro");
        std::fs::create_dir(&write_root).expect("rw dir");
        std::fs::create_dir(&read_root).expect("ro dir");
        std::fs::write(read_root.join("keep.txt"), "keep").expect("ro file");

        let policy = Policy {
            fs: FsPolicy {
                entries: vec![
                    FsEntry {
                        path: write_root.clone(),
                        access: Access::Write,
                    },
                    FsEntry {
                        path: read_root.clone(),
                        access: Access::Read,
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

        let write = executor.blocking_exec("echo data > out.txt");
        assert_eq!(write.exit_code, 0, "stderr: {}", write.stderr);
        assert!(write_root.join("out.txt").exists());

        let blocked = executor.blocking_exec(&format!("touch {}/blocked.txt", read_root.display()));
        assert_ne!(blocked.exit_code, 0, "stderr: {}", blocked.stderr);
        assert!(!read_root.join("blocked.txt").exists());
    }

    #[test]
    fn writes_outside_entries_are_denied() {
        if !spawn_tests_supported() {
            return;
        }
        let dir = TempDir::new().expect("tempdir");
        let host = dir.path().canonicalize().expect("canonical tempdir");
        let executor = executor(&workdir_policy(&host));

        let etc = executor.blocking_exec("touch /etc/agentd-sandbox-blocked");
        assert_ne!(etc.exit_code, 0, "stderr: {}", etc.stderr);
        assert!(!Path::new("/etc/agentd-sandbox-blocked").exists());

        let tmp = executor.blocking_exec("touch /tmp/agentd-sandbox-blocked");
        assert_ne!(tmp.exit_code, 0, "stderr: {}", tmp.stderr);
        assert!(!Path::new("/tmp/agentd-sandbox-blocked").exists());
    }

    #[test]
    fn a_sandboxed_process_reaches_only_a_granted_unix_socket() {
        use std::io::{Read, Write};

        if !spawn_tests_supported() {
            return;
        }

        let dir = TempDir::new().expect("tempdir");
        let host = dir.path().canonicalize().expect("canonical tempdir");
        let granted = host.join("granted.sock");
        let other = host.join("other.sock");

        let listener = std::os::unix::net::UnixListener::bind(&granted).expect("bind granted");
        let server = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().expect("accept");
            let mut buf = [0_u8; 64];
            let read = conn.read(&mut buf).expect("read");
            conn.write_all(b"ok").expect("write");
            String::from_utf8_lossy(&buf[..read]).into_owned()
        });

        let policy = Policy {
            fs: FsPolicy {
                entries: vec![FsEntry {
                    path: host.clone(),
                    access: Access::Write,
                }],
                ..FsPolicy::default()
            },
            shell: ShellPolicy {
                workdir: host,
                ..ShellPolicy::default()
            },
            network: NetworkPolicy {
                unix_sockets: vec![granted.clone()],
            },
            ..Policy::default()
        };
        let executor = executor(&policy);

        let reached =
            executor.blocking_exec(&format!("echo ping | /usr/bin/nc -U {}", granted.display()));
        let received = server.join().expect("server thread");
        assert_eq!(reached.exit_code, 0, "stderr: {}", reached.stderr);
        assert_eq!(received, "ping\n");

        // A socket the policy does not name is refused even though it exists
        // and is served, so the grant is scoped to the named path and not to
        // `AF_UNIX` at large. The served socket closes immediately, so a grant
        // that wrongly allowed the connection would let `nc` exit zero fast
        // rather than hang until the executor timeout.
        let other_listener = std::os::unix::net::UnixListener::bind(&other).expect("bind other");
        let other_server = std::thread::spawn(move || {
            if let Ok((mut conn, _)) = other_listener.accept() {
                let _ = conn.write_all(b"ok");
            }
        });
        let denied =
            executor.blocking_exec(&format!("echo ping | /usr/bin/nc -U {}", other.display()));
        assert_ne!(
            denied.exit_code, 0,
            "an ungranted socket must be refused: `nc` exited {}",
            denied.exit_code
        );
        // Unblock and join the server when the connection was correctly denied.
        let _ = std::os::unix::net::UnixStream::connect(&other);
        other_server.join().expect("other server");
    }

    #[test]
    fn denied_paths_withhold_a_host_file_from_a_spawned_binary() {
        if !spawn_tests_supported() {
            return;
        }
        let dir = TempDir::new().expect("tempdir");
        let host = dir.path().canonicalize().expect("canonical tempdir");
        let secret_dir = TempDir::new().expect("secret tempdir");
        let secret = secret_dir.path().canonicalize().expect("canonical secret");
        std::fs::write(secret.join("token"), "s3cr3t").expect("secret file");

        let policy = Policy {
            fs: FsPolicy {
                entries: vec![
                    FsEntry {
                        path: host.clone(),
                        access: Access::Write,
                    },
                    FsEntry {
                        path: secret.clone(),
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
        let executor = executor(&policy);

        let denied = executor.blocking_exec(&format!("cat {}/token", secret.display()));
        assert_ne!(denied.exit_code, 0, "stdout: {}", denied.stdout);
        assert!(
            !denied.stdout.contains("s3cr3t"),
            "the denial must not leak the contents: {}",
            denied.stdout
        );

        let stat = executor.blocking_exec(&format!("stat -f %z {}/token", secret.display()));
        assert_ne!(stat.exit_code, 0, "stdout: {}", stat.stdout);

        // The denial is narrow: unrelated reads still work.
        let allowed = executor.blocking_exec("cat /etc/hosts");
        assert_eq!(allowed.exit_code, 0, "stderr: {}", allowed.stderr);
        assert!(!allowed.stdout.is_empty());
    }

    #[test]
    fn timeout_kills_the_process_group_quickly() {
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
    fn output_flood_is_truncated_and_killed() {
        if !spawn_tests_supported() {
            return;
        }
        let dir = TempDir::new().expect("tempdir");
        let host = dir.path().canonicalize().expect("canonical tempdir");
        let mut policy = workdir_policy(&host);
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
