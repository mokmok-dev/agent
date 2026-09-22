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
use std::time::Duration;

use agentd_events::LogEntry;
use agentd_events::agent::AGENT_CONVERSATION_STARTED;
use agentd_sandbox::{Access, FsEntry, FsPolicy, Policy, ShellPolicy};
use serde_json::Value;
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::RecvError;

use crate::session::{SESSION_EXITED, SESSION_FAILED};

/// How long to wait for the agent's session announcement before printing the
/// publish command without a conversation id.
const SESSION_ANNOUNCE_TIMEOUT: Duration = Duration::from_secs(5);

/// Waits on `events` for the announcement of the session this `up` launched and
/// returns the conversation id it carries.
///
/// The agent picks the id itself, so the announcement is the only way `up` can
/// learn which conversation its session serves; pinning it means the printed
/// command reaches *that* session even when the database already holds several
/// for the workdir. The wait is bounded, so an agent that starts but never
/// connects does not hold up the operator; the caller prints a usable command
/// either way, and the reason it could not be pinned is logged here.
///
/// Returns `None` when the agent's session ends without announcing, when the
/// subscription closes, or when `session_id` never announces within
/// [`SESSION_ANNOUNCE_TIMEOUT`].
pub async fn await_conversation(
    events: &mut broadcast::Receiver<LogEntry>,
    workdir: &str,
    session_id: &str,
) -> Option<String> {
    let deadline = tokio::time::Instant::now() + SESSION_ANNOUNCE_TIMEOUT;
    loop {
        match tokio::time::timeout_at(deadline, events.recv()).await {
            Ok(Ok(entry))
                if matches!(entry.event.r#type.as_str(), SESSION_EXITED | SESSION_FAILED)
                    && entry.event.data.get("session_id").and_then(Value::as_str)
                        == Some(session_id) =>
            {
                // The session is gone, so no announcement is coming and the
                // printed command cannot reach it; saying "still starting"
                // would misattribute the cause. `session.*` data carries no
                // workdir, so this keys on `session_id` alone: with the default
                // log shared by two `up` runs that both use the default
                // `--session-id agent`, an *other* instance's session ending in
                // this window is read as ours.
                tracing::warn!(
                    %session_id,
                    "the session ended before it announced a conversation; the printed \
                     command has no conversation to target",
                );
                return None;
            },
            Ok(Ok(entry)) if entry.event.r#type == AGENT_CONVERSATION_STARTED => {
                let data = &entry.event.data;
                // Another `up` may share the log while serving a different
                // socket and workdir; only this workspace's session is ours.
                if data.get("workdir").and_then(Value::as_str) != Some(workdir) {
                    continue;
                }
                if let Some(conversation) = data.get("conversation_id").and_then(Value::as_str) {
                    return Some(conversation.to_owned());
                }
            },
            Ok(Ok(_) | Err(RecvError::Lagged(_))) => {},
            Ok(Err(RecvError::Closed)) => return None,
            Err(_) => {
                tracing::warn!(
                    "the agent has not announced its conversation within {}s; the printed \
                     command leaves --conversation to the most recent one for --workdir",
                    SESSION_ANNOUNCE_TIMEOUT.as_secs(),
                );
                return None;
            },
        }
    }
}

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
    /// Continue the most recent session recorded for `--workdir` instead of
    /// starting a new one. A workdir with no recorded session starts a new one
    /// rather than failing, so this is safe on a first-ever run.
    #[arg(long)]
    pub resume: bool,
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
            resume: false,
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
        resume: bool,
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
        if resume {
            parts.push(String::from("--resume"));
        }
        parts.join(" ")
    }

    /// The `agentd-publish` command an operator can run to prompt the session.
    ///
    /// Every path is named explicitly: the session database in particular is
    /// neither `agentd-publish`'s default database nor derived from
    /// `--workdir`, and the user token is repeated from the XDG default so the
    /// command can be read without knowing that default.
    #[must_use]
    pub fn publish_command(
        &self,
        conversation: Option<&str>,
    ) -> String {
        let mut parts = vec![
            String::from("agentd-publish"),
            String::from("--socket"),
            quote(&self.socket.to_string_lossy()),
            String::from("--token-file"),
            quote(&agentd_events::paths::default_user_token().to_string_lossy()),
            String::from("--db"),
            quote(&self.session_db.to_string_lossy()),
            String::from("--workdir"),
            quote(&self.workdir.to_string_lossy()),
        ];
        if let Some(conversation) = conversation {
            parts.push(String::from("--conversation"));
            parts.push(quote(conversation));
        }
        parts.push(String::from("--inbox"));
        parts.push(String::from("'<your prompt>'"));
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
    use super::{
        AGENT_CONVERSATION_STARTED, SESSION_EXITED, UpError, UpPaths, await_conversation, quote,
    };
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

        let command = layout.agent_command(None, false);
        assert_eq!(
            command,
            "/usr/bin/agentd-agent --socket /run/agentd/agentd.sock \
             --token-file /cfg/agent.token --db /data/agent.db --workdir /ws"
        );
        assert!(
            layout
                .agent_command(Some("fast"), false)
                .ends_with("--model fast")
        );
    }

    #[test]
    fn resume_is_forwarded_unconditionally() {
        let layout = layout(
            PathBuf::from("/ws"),
            PathBuf::from("/data/agent.db"),
            PathBuf::from("/cfg/agent.token"),
        );

        // Whether a session exists for the workdir is the agent's question to
        // answer (it falls back to a new session), so `up` forwards the flag
        // without inspecting the database.
        assert!(layout.agent_command(None, true).ends_with("--resume"));
        assert!(!layout.agent_command(None, false).contains("--resume"));
    }

    #[test]
    fn the_publish_command_names_the_session_database_and_the_user_token() {
        let layout = layout(
            PathBuf::from("/ws"),
            PathBuf::from("/data/agent.db"),
            PathBuf::from("/cfg/agent.token"),
        );

        let command = layout.publish_command(Some("018f6b2e-7e5c-7000-8000-000000000000"));
        assert!(command.starts_with("agentd-publish "));
        assert!(command.contains("--socket /run/agentd/agentd.sock"));
        assert!(command.contains("--db /data/agent.db"));
        assert!(command.contains("--workdir /ws"));
        assert!(command.contains("--conversation 018f6b2e-7e5c-7000-8000-000000000000"));
        assert!(command.ends_with("--inbox '<your prompt>'"));
        assert!(
            command.contains(&format!(
                "--token-file {}",
                agentd_events::paths::default_user_token().display()
            )),
            "{command}"
        );
    }

    #[test]
    fn the_publish_command_omits_an_unknown_conversation() {
        let layout = layout(
            PathBuf::from("/ws"),
            PathBuf::from("/data/agent.db"),
            PathBuf::from("/cfg/agent.token"),
        );

        assert!(!layout.publish_command(None).contains("--conversation"));
    }

    #[tokio::test(start_paused = true)]
    async fn await_conversation_finds_the_announcement_for_this_workdir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = agentd_events::EventLog::open(dir.path().join("events.jsonl")).expect("log");
        let mut events = log.subscribe();
        log.publish(agentd_events::Event::new(
            "agent.turn.started",
            serde_json::json!({}),
        ))
        .await
        .expect("publish");
        // Another instance's session on the shared log must not win the race.
        log.publish(agentd_events::Event::new(
            AGENT_CONVERSATION_STARTED,
            serde_json::json!({ "conversation_id": "other", "workdir": "/elsewhere" }),
        ))
        .await
        .expect("publish");
        // A malformed announcement without an id is skipped, not accepted.
        log.publish(agentd_events::Event::new(
            AGENT_CONVERSATION_STARTED,
            serde_json::json!({ "workdir": "/ws" }),
        ))
        .await
        .expect("publish");
        log.publish(agentd_events::Event::new(
            AGENT_CONVERSATION_STARTED,
            serde_json::json!({ "conversation_id": "mine", "workdir": "/ws" }),
        ))
        .await
        .expect("publish");

        assert_eq!(
            await_conversation(&mut events, "/ws", "agent")
                .await
                .as_deref(),
            Some("mine")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn await_conversation_gives_up_at_the_deadline() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = agentd_events::EventLog::open(dir.path().join("events.jsonl")).expect("log");
        let mut events = log.subscribe();

        assert!(
            await_conversation(&mut events, "/ws", "agent")
                .await
                .is_none()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn await_conversation_gives_up_when_its_own_session_ends_first() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = agentd_events::EventLog::open(dir.path().join("events.jsonl")).expect("log");
        let mut events = log.subscribe();
        // Another session ending must not stop the wait for ours.
        log.publish(agentd_events::Event::new(
            SESSION_EXITED,
            serde_json::json!({ "session_id": "other" }),
        ))
        .await
        .expect("publish");
        log.publish(agentd_events::Event::new(
            SESSION_EXITED,
            serde_json::json!({ "session_id": "agent" }),
        ))
        .await
        .expect("publish");

        assert!(
            await_conversation(&mut events, "/ws", "agent")
                .await
                .is_none()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn await_conversation_gives_up_when_the_subscription_closes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = agentd_events::EventLog::open(dir.path().join("events.jsonl")).expect("log");
        let mut events = log.subscribe();
        drop(log);

        assert!(
            await_conversation(&mut events, "/ws", "agent")
                .await
                .is_none()
        );
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
