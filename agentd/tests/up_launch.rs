//! End-to-end coverage of `agentd up`'s derivation: the policy and the agent
//! command it builds from a single `--workdir`, and the confinement that policy
//! actually produces when a real command runs under it.
//!
//! The derivation is unit-tested in `agentd::up`; this file exercises it through
//! the public API against real files and a real sandbox, so the security
//! properties (the agent can read its own token but not the daemon's others, and
//! its shell tool cannot escape the workspace) are checked against the OS
//! backends rather than only the policy shape.
//!
//! The helpers use `expect` like the other integration tests; the workspace
//! `allow-*-in-tests` clippy configuration does not see integration test files,
//! so it is replicated here.

#![expect(
    clippy::expect_used,
    reason = "integration tests use expect for setup and assertions"
)]

use agentd::up::{UpArgs, UpPaths};
use agentd_events::EventLog;
use agentd_sandbox::{Access, Sandbox};
use std::path::{Path, PathBuf};

/// Builds an [`UpArgs`] whose only meaningful path is the workspace; the token
/// and agent defaults are overridden so the test does not read the real XDG
/// configuration.
fn args(
    workdir: &Path,
    agent_token: &Path,
    session_db: &Path,
) -> UpArgs {
    UpArgs {
        workdir: workdir.to_path_buf(),
        socket: Some(PathBuf::from("/run/agentd/agentd.sock")),
        log_path: None,
        token_file: None,
        providers_config: None,
        agent_token_file: Some(agent_token.to_path_buf()),
        agent: None,
        model: None,
        session_id: String::from("test"),
        session_db: Some(session_db.to_path_buf()),
        session_max_restarts: 0,
        session_lifetime_secs: None,
    }
}

/// A workspace, a session directory, and a token file, wired into a `UpPaths`.
fn fixture(dir: &Path) -> UpPaths {
    let workdir = dir.join("workspace");
    std::fs::create_dir_all(&workdir).expect("workspace");
    let session_db = dir.join("session").join("agent.db");
    std::fs::create_dir_all(session_db.parent().expect("parent")).expect("session dir");
    let token = dir.join("agent.token");
    std::fs::write(&token, "agent-secret").expect("token");

    UpPaths::resolve(&args(&workdir, &token, &session_db)).expect("layout should resolve")
}

/// A `UpPaths` whose daemon capability files sit beside the agent's token, so the
/// denied set is populated. Returns the layout and the daemon's token file.
fn fixture_with_daemon_capabilities(dir: &Path) -> (UpPaths, PathBuf) {
    let workdir = dir.join("workspace");
    std::fs::create_dir_all(&workdir).expect("workspace");
    let session_db = dir.join("session").join("agent.db");
    std::fs::create_dir_all(session_db.parent().expect("parent")).expect("session dir");
    let agent_token = dir.join("agent.token");
    std::fs::write(&agent_token, "agent-secret").expect("agent token");
    let daemon_token = dir.join("tokens.json");
    std::fs::write(&daemon_token, r#"{"tokens":[]}"#).expect("daemon token");
    let admin_token = dir.join("admin.token");
    std::fs::write(&admin_token, "admin-secret").expect("admin token");

    let mut args = args(&workdir, &agent_token, &session_db);
    args.token_file = Some(daemon_token.clone());
    (
        UpPaths::resolve(&args).expect("layout should resolve"),
        daemon_token,
    )
}

#[test]
fn the_derived_command_passes_every_absolute_path() {
    let dir = tempfile::tempdir().expect("tempdir");
    let layout = fixture(dir.path());

    let command = layout.agent_command(Some("fast"));

    // Every path the sandbox rewrites `HOME` out from under must be absolute.
    for path in [
        &layout.agent_binary,
        &layout.socket,
        &layout.agent_token,
        &layout.session_db,
        &layout.workdir,
    ] {
        assert!(
            command.contains(&path.display().to_string()),
            "the command must name {}: {command}",
            path.display()
        );
    }
    assert!(command.contains("--model fast"));
}

#[test]
fn the_workdir_is_a_write_root_and_egress_is_denied() {
    let dir = tempfile::tempdir().expect("tempdir");
    let layout = fixture(dir.path());

    let policy = layout.policy().expect("policy should derive");

    assert_eq!(policy.shell.workdir, layout.workdir);
    assert!(
        policy
            .fs
            .entries
            .iter()
            .any(|entry| entry.path == layout.workdir && entry.access == Access::Write),
        "the workspace must be writable"
    );
    // The built-in agent reaches inference over the daemon's Unix socket, so it
    // needs no IP egress and no loopback.
    assert!(policy.network.proxy.is_none());
    assert!(!policy.network.loopback);
}

#[tokio::test]
async fn the_agent_may_read_its_own_token_and_nothing_else_of_the_daemon() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (layout, daemon_token) = fixture_with_daemon_capabilities(dir.path());

    let policy = layout.policy().expect("policy should derive");
    let log = EventLog::open(dir.path().join("events.jsonl")).expect("log");
    let sandbox = Sandbox::new(&policy, log, "test").expect("sandbox should build");

    let readable = sandbox
        .exec(&format!("cat {}", layout.agent_token.display()))
        .await
        .expect("exec");
    assert_eq!(
        readable.exit_code, 0,
        "the agent's own token must be readable: {}",
        readable.stderr
    );
    assert!(readable.stdout.contains("agent-secret"));

    // The daemon's token file, which the agent must never read: it holds every
    // capability, including `authority`.
    let forbidden = sandbox
        .exec(&format!("cat {}", daemon_token.display()))
        .await
        .expect("exec");
    assert_ne!(
        forbidden.exit_code, 0,
        "the daemon's token file must not be readable: {}",
        forbidden.stdout
    );
    assert!(
        forbidden.stdout.trim().is_empty(),
        "the daemon's token contents must not leak"
    );
}

#[tokio::test]
async fn the_workspace_is_writable_and_the_host_is_not() {
    let dir = tempfile::tempdir().expect("tempdir");
    let layout = fixture(dir.path());
    let policy = layout.policy().expect("policy should derive");
    let log = EventLog::open(dir.path().join("events.jsonl")).expect("log");
    let sandbox = Sandbox::new(&policy, log, "test").expect("sandbox should build");

    let inside = layout.workdir.join("written.txt");
    let write = sandbox
        .exec(&format!("printf ok > {}", inside.display()))
        .await
        .expect("exec");
    assert_eq!(
        write.exit_code, 0,
        "the workspace must be writable: {}",
        write.stderr
    );
    assert_eq!(
        std::fs::read_to_string(&inside).expect("the file was written"),
        "ok"
    );

    // A path outside every write root is not writable.
    let host = dir.path().join("host.txt");
    let escape = sandbox
        .exec(&format!("printf no > {}", host.display()))
        .await
        .expect("exec");
    assert_ne!(
        escape.exit_code, 0,
        "a path outside the write roots must not be writable: {}",
        escape.stdout
    );
    assert!(!host.exists(), "the host file must not have been created");
}

#[test]
fn a_missing_token_file_is_reported_before_anything_starts() {
    let dir = tempfile::tempdir().expect("tempdir");
    let workdir = dir.path().join("workspace");
    std::fs::create_dir_all(&workdir).expect("workspace");

    let error = UpPaths::resolve(&args(
        &workdir,
        &dir.path().join("absent.token"),
        &dir.path().join("session").join("agent.db"),
    ))
    .expect_err("a missing agent token must fail");

    assert!(matches!(error, agentd::up::UpError::MissingAgentToken(_)));
}
