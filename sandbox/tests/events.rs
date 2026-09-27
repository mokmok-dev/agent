//! End-to-end tests that produce a violation event from a real denied command.
//!
//! The classifier is unit-tested against the kernel's exact messages; these tests
//! prove those messages are what a real confined command actually prints. Each
//! one detects the backend first and returns early when this host cannot build a
//! namespace, the same condition under which the daemon would refuse to spawn.
#![expect(
    clippy::expect_used,
    reason = "integration test code may panic when a fixture fails"
)]

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use sandbox::events::{Event, Reason, SandboxContext, ViolationKind, classify};
use sandbox::executor::{ExecOutcome, ExecRequest, Scratch, run};
use sandbox::filesystem::Backend;
use sandbox::policy::{EnvVar, FsEntry, FsPolicy, Limits, Policy, ShellPolicy};

/// A temp tree with a write root, removed on drop.
struct Tree(PathBuf);

impl Tree {
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "sandbox-events-{tag}-{}-{unique}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("work")).expect("creates the write root");
        fs::create_dir_all(root.join("outside")).expect("creates the outside dir");
        Self(root)
    }

    fn work(&self) -> PathBuf {
        self.0.join("work")
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The backend, or `None` when this host cannot confine a command.
fn backend_or_skip() -> Option<Backend> {
    let path = std::env::var("PATH").unwrap_or_default();
    let backend = Backend::detect(&path, &[]);
    backend.is_supported().then_some(backend)
}

/// A policy that writes only inside `tree`'s `work` root.
fn policy(tree: &Tree) -> Policy {
    Policy {
        fs: FsPolicy {
            entries: vec![FsEntry::write(tree.work())],
            protected: Vec::new(),
        },
        shell: ShellPolicy {
            env: vec![EnvVar::new(
                "PATH",
                std::env::var("PATH").unwrap_or_else(|_| "/bin:/usr/bin".to_owned()),
            )],
            workdir: tree.work(),
        },
        limits: Limits {
            timeout_millis: 10_000,
            max_output_bytes: 1024 * 1024,
        },
        ..Policy::default()
    }
}

/// Run `/bin/sh -c <script>` under `policy`.
fn sh(
    backend: &Backend,
    policy: &Policy,
    scratch: &Scratch,
    script: &str,
) -> ExecOutcome {
    let request = ExecRequest {
        program: OsString::from("/bin/sh"),
        args: vec![OsString::from("-c"), OsString::from(script)],
        policy: policy.clone(),
    };
    run(backend, &request, scratch).expect("the confined command runs")
}

#[test]
fn a_real_read_only_write_classifies_as_a_filesystem_violation() {
    let Some(backend) = backend_or_skip() else {
        return;
    };
    let tree = Tree::new("read-only");
    let scratch = Scratch::new(&tree.work()).expect("scratch");
    let policy = policy(&tree);
    let target = tree.0.join("outside/evil.txt");

    let outcome = sh(
        &backend,
        &policy,
        &scratch,
        &format!("echo x > {}", target.display()),
    );

    // The classifier recovers the kernel's denial from the real stderr.
    let found = classify(&outcome.stderr);
    assert_eq!(
        found.len(),
        1,
        "stderr was: {}",
        String::from_utf8_lossy(&outcome.stderr)
    );
    assert_eq!(found[0].kind, ViolationKind::Filesystem);
    assert_eq!(found[0].reason, Reason::ReadOnlyFileSystem);
    assert_eq!(
        found[0].denied_path.as_deref(),
        Some(target.to_str().expect("utf-8"))
    );
}

#[test]
fn a_real_denied_read_classifies_as_permission_denied() {
    let Some(backend) = backend_or_skip() else {
        return;
    };
    let tree = Tree::new("denied-read");
    fs::write(tree.work().join("secret.txt"), b"hidden").expect("writes");
    let scratch = Scratch::new(&tree.work()).expect("scratch");
    let mut policy = policy(&tree);
    policy
        .fs
        .entries
        .push(FsEntry::deny(tree.work().join("secret.txt")));

    let outcome = sh(
        &backend,
        &policy,
        &scratch,
        &format!("cat {}/secret.txt", tree.work().display()),
    );

    let found = classify(&outcome.stderr);
    assert_eq!(
        found.len(),
        1,
        "stderr was: {}",
        String::from_utf8_lossy(&outcome.stderr)
    );
    assert_eq!(found[0].reason, Reason::PermissionDenied);
}

#[test]
fn a_real_network_attempt_classifies_as_network_unreachable() {
    let Some(backend) = backend_or_skip() else {
        return;
    };
    // Reaching an address needs a shell with `/dev/tcp`; that is a bash
    // extension, so the test uses `/bin/bash` when present and skips otherwise.
    // A dash-only host cannot express the attempt, and the classifier's network
    // arm is still unit-tested against the kernel's message.
    if !Path::new("/bin/bash").exists() {
        return;
    }
    let tree = Tree::new("network");
    let scratch = Scratch::new(&tree.work()).expect("scratch");
    let policy = policy(&tree);

    // A private network namespace has no route out.
    let request = ExecRequest {
        program: OsString::from("/bin/bash"),
        args: vec![
            OsString::from("-c"),
            OsString::from("cat < /dev/tcp/1.1.1.1/443"),
        ],
        policy,
    };
    let outcome = run(&backend, &request, &scratch).expect("the confined command runs");

    let found = classify(&outcome.stderr);
    assert!(
        found.iter().any(|v| v.kind == ViolationKind::Network),
        "stderr was: {}",
        String::from_utf8_lossy(&outcome.stderr)
    );
    let event = Event::violations(&outcome, &SandboxContext::new("sbx-1", "connect"));
    assert_eq!(event.len(), found.len());
}

#[test]
fn a_successful_command_produces_no_violation() {
    let Some(backend) = backend_or_skip() else {
        return;
    };
    let tree = Tree::new("clean");
    let scratch = Scratch::new(&tree.work()).expect("scratch");
    let policy = policy(&tree);

    let outcome = sh(&backend, &policy, &scratch, "echo fine");

    assert_eq!(outcome.code, Some(0));
    assert!(
        Event::violations(&outcome, &SandboxContext::new("sbx-1", "echo")).is_empty(),
        "a clean exit is not a violation"
    );
}
