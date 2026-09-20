//! One-shot launch: derive the sandbox policy and the agent's command from a
//! single `--workdir`.
//!
//! Bringing the stack up by hand today means writing a sandbox policy JSON and
//! an absolute `--session-command` that repeats the daemon's socket, token, and
//! database paths (the sandbox rewrites `HOME`, so the node cannot discover
//! them). Nothing about that is interesting: every value is already known to
//! the daemon, and only the workspace is a real choice.
//!
//! So [`UpArgs`] carries the workspace and the derivation lives here. The
//! generated policy writes exactly two roots — the workspace and the session
//! database directory — reads the agent's own token file and the agent binary
//! itself, and denies the daemon's other capability and credential files. The
//! built-in agent asks the daemon for inference over its Unix socket, so no
//! egress and no loopback are granted.

use std::path::{Path, PathBuf};

use agentd_sandbox::{Access, FsEntry, FsPolicy, Policy, ShellPolicy};

/// The `agentd up` arguments.
#[derive(Debug, clap::Args)]
#[command(name = "up")]
pub struct UpArgs {
    /// The workspace the agent works in: the only directory it may write.
    #[arg(long)]
    pub workdir: PathBuf,
    /// The event WebSocket Unix socket. Defaults to the XDG runtime path.
    #[arg(long)]
    pub socket: Option<PathBuf>,
    /// The durable JSONL event log. Defaults to the XDG data path.
    #[arg(long)]
    pub log_path: Option<PathBuf>,
    /// The daemon's bearer token file. Defaults to `~/.config/agentd/tokens.json`.
    #[arg(long)]
    pub token_file: Option<PathBuf>,
    /// The provider config JSON enabling real models. Defaults to
    /// `~/.config/agentd/providers.json` when it exists.
    #[arg(long)]
    pub providers_config: Option<PathBuf>,
    /// The token file the confined agent presents; it needs the read, publish,
    /// and infer claims. Defaults to `~/.config/agentd/agent.token`.
    #[arg(long)]
    pub agent_token_file: Option<PathBuf>,
    /// The `agentd-agent` binary to run confined. Defaults to the sibling of
    /// this binary, then `PATH`.
    #[arg(long)]
    pub agent: Option<PathBuf>,
    /// The model the agent asks the daemon for (an alias or `provider/model`).
    /// Without it the daemon's `default_model` is used.
    #[arg(long)]
    pub model: Option<String>,
    /// The session id recorded on `session.*` events.
    #[arg(long, default_value = "agent")]
    pub session_id: String,
    /// The SQLite conversation projection. Its directory is made writable.
    /// Defaults to `~/.local/share/agentd/sessions/<session-id>/agent.db`.
    #[arg(long)]
    pub session_db: Option<PathBuf>,
    /// Restart a crashed agent up to this many times.
    #[arg(long, default_value_t = 0)]
    pub session_max_restarts: u32,
    /// Kill the agent after this many seconds.
    #[arg(long)]
    pub session_lifetime_secs: Option<u64>,
}

impl Default for UpArgs {
    /// The CLI defaults, so a programmatic caller can write
    /// `UpArgs { workdir, ..UpArgs::default() }`.
    fn default() -> Self {
        Self {
            workdir: PathBuf::new(),
            socket: None,
            log_path: None,
            token_file: None,
            providers_config: None,
            agent_token_file: None,
            agent: None,
            model: None,
            session_id: String::from("agent"),
            session_db: None,
            session_max_restarts: 0,
            session_lifetime_secs: None,
        }
    }
}

/// The absolute paths `up` derives the policy and the agent command from.
///
/// Constructed by [`UpPaths::resolve`], which is the only supported entry point:
/// every field must be absolute and canonical for the derived policy and command
/// to be correct, so the struct is not exhaustively constructible elsewhere.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct UpPaths {
    /// The canonical workspace: a write root and the agent's working directory.
    pub workdir: PathBuf,
    /// The daemon's event socket, granted to the agent's policy.
    pub socket: PathBuf,
    /// The agent's canonical bearer token file: its only readable capability.
    pub agent_token: PathBuf,
    /// The daemon's own token file, which holds every capability and must stay
    /// unreadable to the confined agent.
    pub daemon_token: PathBuf,
    /// The canonical `agentd-agent` binary to run confined.
    pub agent_binary: PathBuf,
    /// The session's SQLite projection; its directory is the second write root.
    pub session_db: PathBuf,
}

impl UpPaths {
    /// Resolves the paths for `args`, creating the session directory.
    ///
    /// # Errors
    ///
    /// Returns [`UpError::Workdir`] when `--workdir` is missing or not a
    /// directory, [`UpError::MissingAgentToken`] when the agent's token file
    /// does not exist, [`UpError::MissingAgent`] when no `agentd-agent` binary
    /// can be found, and [`UpError::Io`] when a path cannot be resolved or a
    /// directory cannot be created.
    pub fn resolve(args: &UpArgs) -> Result<Self, UpError> {
        let workdir =
            canonical_dir(&args.workdir).ok_or_else(|| UpError::Workdir(args.workdir.clone()))?;

        let socket = args
            .socket
            .clone()
            .unwrap_or_else(agentd_events::paths::default_socket);

        let agent_token_path = args
            .agent_token_file
            .clone()
            .unwrap_or_else(|| agentd_events::paths::config_dir().join("agent.token"));
        let agent_token = agent_token_path
            .canonicalize()
            .map_err(|_| UpError::MissingAgentToken(agent_token_path.clone()))?;

        let daemon_token = args
            .token_file
            .clone()
            .unwrap_or_else(agentd_events::paths::default_tokens);

        let agent_binary = resolve_agent(args.agent.as_deref())?;

        let session_db = args.session_db.clone().unwrap_or_else(|| {
            agentd_events::paths::data_dir()
                .join("sessions")
                .join(&args.session_id)
                .join("agent.db")
        });
        if let Some(parent) = session_db
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)?;
        }

        Ok(Self {
            workdir,
            socket,
            agent_token,
            daemon_token,
            agent_binary,
            session_db,
        })
    }

    /// The `agentd-agent` command, with every path absolute.
    ///
    /// The sandbox rewrites `HOME` to its scratch directory, so the agent
    /// cannot discover the daemon's socket, token, or database from the XDG
    /// defaults; all three are passed explicitly.
    #[must_use]
    pub fn agent_command(
        &self,
        model: Option<&str>,
    ) -> String {
        let mut parts = vec![
            quote(&self.agent_binary.to_string_lossy()),
            String::from("--socket"),
            quote(&self.socket.to_string_lossy()),
            String::from("--token-file"),
            quote(&self.agent_token.to_string_lossy()),
            String::from("--db"),
            quote(&self.session_db.to_string_lossy()),
            String::from("--workdir"),
            quote(&self.workdir.to_string_lossy()),
        ];
        if let Some(model) = model {
            parts.push(String::from("--model"));
            parts.push(quote(model));
        }
        parts.join(" ")
    }

    /// Builds the sandbox policy these paths imply.
    ///
    /// Writable: the workspace and the session database's directory. Readable:
    /// the agent's own token file and the `agentd-agent` binary itself — a
    /// system directory is already granted, so the binary entry is needed only
    /// for a binary outside the system roots, such as a `cargo` build, and it
    /// names the file rather than its directory so nothing beside it becomes
    /// readable. Denied: the daemon's other capability files (`tokens.json`,
    /// `*.token`) and its `providers.json`, which holds provider credentials —
    /// reads are broad on macOS and under `bwrap`, so they are removed
    /// explicitly.
    ///
    /// The denials are a snapshot of the files that exist when this is called;
    /// a capability file created afterwards is not denied.
    ///
    /// # Errors
    ///
    /// Returns [`UpError::MissingSessionDir`] when the database has no parent
    /// directory and [`UpError::Policy`] when the result fails validation.
    pub fn policy(&self) -> Result<Policy, UpError> {
        let session_dir = self
            .session_db
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .ok_or_else(|| UpError::MissingSessionDir(self.session_db.clone()))?;
        let session_dir = session_dir
            .canonicalize()
            .map_err(|_| UpError::MissingSessionDir(self.session_db.clone()))?;

        let entries = vec![
            FsEntry {
                path: self.workdir.clone(),
                access: Access::Write,
            },
            FsEntry {
                path: session_dir,
                access: Access::Write,
            },
            FsEntry {
                path: self.agent_token.clone(),
                access: Access::Read,
            },
            // The file, not its directory: the parent of a `cargo`-built binary
            // is the whole `target/<profile>` tree, and granting it broadly would
            // both expose unrelated files and swallow a deny nested under it.
            FsEntry {
                path: self.agent_binary.clone(),
                access: Access::Read,
            },
        ];

        let policy = Policy {
            fs: FsPolicy {
                entries: entries
                    .into_iter()
                    .chain(denied_capabilities(self))
                    .collect(),
                ..FsPolicy::default()
            },
            shell: ShellPolicy {
                workdir: self.workdir.clone(),
                ..ShellPolicy::default()
            },
            ..Policy::default()
        };
        policy.validate()?;
        Ok(policy)
    }
}

/// The daemon-side files the confined agent must not read.
///
/// The daemon's token file (`--token-file`) holds every capability, including
/// `authority`; the other capability files beside the agent's own token —
/// `tokens.json`, `user.token`, `admin.token` — are further capabilities; and
/// `providers.json` names provider credentials. The agent's own token file is
/// exempt: it is the capability the agent legitimately holds.
///
/// Reads are broad on macOS and under `bwrap`, so these are removed explicitly;
/// under the Landlock fallback they are outside the allowlist already and the
/// denials simply never apply.
fn denied_capabilities(layout: &UpPaths) -> Vec<FsEntry> {
    let mut candidates: Vec<PathBuf> = vec![layout.daemon_token.clone()];

    // The other capabilities that sit beside the agent's token; a deployment
    // that points `--agent-token-file` elsewhere has no siblings to deny.
    if let Some(dir) = layout.agent_token.parent() {
        candidates.push(dir.join(crate::init::PROVIDERS_FILE));
        candidates.extend(
            std::fs::read_dir(dir)
                .into_iter()
                .flatten()
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| {
                    path.extension()
                        .is_some_and(|extension| extension == "token")
                }),
        );
    }

    candidates
        .into_iter()
        .filter_map(|path| path.canonicalize().ok())
        .filter(|path| path != &layout.agent_token)
        .map(|path| FsEntry {
            path,
            access: Access::Deny,
        })
        .collect()
}

/// Resolves the `agentd-agent` binary: an explicit path, then a sibling of this
/// binary, then `PATH`.
fn resolve_agent(explicit: Option<&Path>) -> Result<PathBuf, UpError> {
    if let Some(path) = explicit {
        return path
            .canonicalize()
            .ok()
            .filter(|path| is_executable(path))
            .ok_or_else(|| UpError::NotExecutable(path.to_path_buf()));
    }
    if let Some(found) = sibling_of_current_exe("agentd-agent").filter(|path| is_executable(path)) {
        return Ok(found);
    }
    find_on_path("agentd-agent").ok_or(UpError::MissingAgent)
}

/// The `name` binary next to this executable, or one directory above it.
///
/// `cargo test` runs from `target/<profile>/deps`, so the parent is tried too.
fn sibling_of_current_exe(name: &str) -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    [dir.join(name), dir.parent()?.join(name)]
        .into_iter()
        .find_map(|candidate| candidate.canonicalize().ok())
}

/// Finds an executable `name` on `PATH`.
fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}

/// Whether `path` is a regular executable file.
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

/// Canonicalizes `path` when it is an existing directory.
fn canonical_dir(path: &Path) -> Option<PathBuf> {
    let canonical = path.canonicalize().ok()?;
    canonical.is_dir().then_some(canonical)
}

/// Quotes `value` for a POSIX shell when it needs it.
fn quote(value: &str) -> String {
    let plain = !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_./:@+".contains(&byte));
    if plain {
        return value.to_owned();
    }
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// Errors returned while deriving the one-shot launch.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum UpError {
    /// The workspace is missing or is not a directory.
    #[error("the workspace {0} is not an accessible directory")]
    Workdir(PathBuf),
    /// The agent's token file does not exist.
    #[error("the agent token file {0} does not exist; run `agentd init` to create it")]
    MissingAgentToken(PathBuf),
    /// The session database has no usable parent directory.
    #[error("the session database {0} has no usable parent directory")]
    MissingSessionDir(PathBuf),
    /// No `agentd-agent` binary could be found.
    #[error("could not find an `agentd-agent` binary next to this one or on PATH; pass --agent")]
    MissingAgent,
    /// The configured agent binary is not executable.
    #[error("the agent binary {0} is not an executable file")]
    NotExecutable(PathBuf),
    /// The derived policy failed validation.
    #[error("the derived sandbox policy is unusable: {0}")]
    Policy(#[from] agentd_sandbox::PolicyError),
    /// A directory could not be created or canonicalized.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::{UpError, UpPaths, quote};
    use agentd_sandbox::Access;
    use std::path::PathBuf;

    fn layout(
        workdir: PathBuf,
        session_db: PathBuf,
        agent_token: PathBuf,
    ) -> UpPaths {
        UpPaths {
            workdir,
            socket: PathBuf::from("/run/agentd/agentd.sock"),
            agent_token,
            daemon_token: PathBuf::from("/cfg/tokens.json"),
            agent_binary: PathBuf::from("/usr/bin/agentd-agent"),
            session_db,
        }
    }

    #[test]
    fn quotes_only_what_a_shell_would_split() {
        assert_eq!(quote("/plain/path-1.2:x@y+z"), "/plain/path-1.2:x@y+z");
        assert_eq!(quote("/with space"), "'/with space'");
        assert_eq!(quote(""), "''");
        assert_eq!(quote("it's"), r"'it'\''s'");
    }

    #[test]
    fn the_command_passes_every_path_absolutely() {
        let layout = layout(
            PathBuf::from("/ws"),
            PathBuf::from("/data/agent.db"),
            PathBuf::from("/cfg/agent.token"),
        );

        let command = layout.agent_command(None);
        assert_eq!(
            command,
            "/usr/bin/agentd-agent --socket /run/agentd/agentd.sock \
             --token-file /cfg/agent.token --db /data/agent.db --workdir /ws"
        );
        assert!(layout.agent_command(Some("fast")).ends_with("--model fast"));
    }

    #[test]
    fn the_workspace_and_the_session_directory_are_the_write_roots() {
        let dir = tempfile::tempdir().expect("tempdir");
        let workdir = dir.path().canonicalize().expect("canonical");
        let session_dir = workdir.join("session");
        std::fs::create_dir(&session_dir).expect("session dir");
        let token = workdir.join("agent.token");
        std::fs::write(&token, "secret").expect("token");

        let policy = layout(
            workdir.clone(),
            session_dir.join("agent.db"),
            token.canonicalize().expect("canonical token"),
        )
        .policy()
        .expect("the derived policy should be valid");

        let writes: Vec<&PathBuf> = policy
            .fs
            .entries
            .iter()
            .filter(|entry| entry.access == Access::Write)
            .map(|entry| &entry.path)
            .collect();
        assert!(writes.contains(&&workdir));
        assert!(
            writes.contains(&&session_dir.canonicalize().expect("canonical session")),
            "the session directory must be writable: {writes:?}"
        );
        assert_eq!(policy.shell.workdir, workdir);
        assert!(policy.network.unix_sockets.is_empty());
        assert!(policy.network.proxy.is_none());
        assert!(!policy.network.loopback);
    }

    #[test]
    fn the_agent_token_is_readable_and_never_denied() {
        let dir = tempfile::tempdir().expect("tempdir");
        let workdir = dir.path().canonicalize().expect("canonical");
        let session_dir = workdir.join("session");
        std::fs::create_dir(&session_dir).expect("session dir");
        let token = workdir.join("agent.token");
        std::fs::write(&token, "secret").expect("token");
        let token = token.canonicalize().expect("canonical token");

        let policy = layout(workdir, session_dir.join("agent.db"), token.clone())
            .policy()
            .expect("policy");

        assert!(
            policy
                .fs
                .entries
                .iter()
                .any(|entry| entry.path == token && entry.access == Access::Read),
            "the agent's token must be readable"
        );
        assert!(
            !policy
                .fs
                .entries
                .iter()
                .any(|entry| entry.path == token && entry.access == Access::Deny),
            "the agent's own token must not be denied"
        );
    }

    #[test]
    fn a_session_database_without_a_parent_is_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let workdir = dir.path().canonicalize().expect("canonical");
        let error = layout(
            workdir,
            PathBuf::from("/"),
            PathBuf::from("/usr/bin/agentd-agent"),
        )
        .policy()
        .expect_err("a parentless database must be rejected");

        assert!(matches!(error, UpError::MissingSessionDir(_)));
    }

    #[test]
    fn the_daemon_capability_files_are_denied() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = dir.path().join("config");
        std::fs::create_dir(&config).expect("config dir");
        let workdir = dir.path().join("workspace");
        std::fs::create_dir(&workdir).expect("workspace");
        let session_dir = dir.path().join("session");
        std::fs::create_dir(&session_dir).expect("session dir");

        let agent_token = config.join("agent.token");
        std::fs::write(&agent_token, "agent-secret").expect("agent token");
        let daemon_tokens = config.join("tokens.json");
        std::fs::write(&daemon_tokens, r#"{"tokens":[]}"#).expect("daemon tokens");
        let admin_token = config.join("admin.token");
        std::fs::write(&admin_token, "admin-secret").expect("admin token");

        let layout = UpPaths {
            workdir: workdir.canonicalize().expect("canonical workspace"),
            socket: PathBuf::from("/run/agentd/agentd.sock"),
            agent_token: agent_token.canonicalize().expect("canonical agent token"),
            daemon_token: daemon_tokens.clone(),
            agent_binary: PathBuf::from("/usr/bin/agentd-agent"),
            session_db: session_dir
                .canonicalize()
                .expect("canonical session")
                .join("agent.db"),
        };
        let policy = layout.policy().expect("policy");

        let denied: Vec<&PathBuf> = policy
            .fs
            .entries
            .iter()
            .filter(|entry| entry.access == Access::Deny)
            .map(|entry| &entry.path)
            .collect();
        assert!(
            denied.contains(
                &&daemon_tokens
                    .canonicalize()
                    .expect("canonical daemon tokens")
            ),
            "the daemon's token file must be denied: {denied:?}"
        );
        assert!(
            denied.contains(&&admin_token.canonicalize().expect("canonical admin token")),
            "the other capabilities must be denied: {denied:?}"
        );
        assert!(
            !denied.contains(&&layout.agent_token),
            "the agent's own token must not be denied"
        );
    }
}
