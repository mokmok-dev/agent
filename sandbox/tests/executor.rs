//! End-to-end tests that spawn a real confined command through bubblewrap.
//!
//! Each test detects the backend first and returns early when this host cannot
//! build a namespace, which is the same condition under which the daemon would
//! refuse to spawn. On such a host the assertions would be meaningless, so
//! skipping is the honest outcome rather than a failure.
#![expect(
    clippy::expect_used,
    reason = "integration test code may panic when a fixture fails"
)]

use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use sandbox::executor::{ExecError, ExecRequest, Scratch, run};
use sandbox::filesystem::{Backend, RenderError};
use sandbox::policy::{EnvVar, FsEntry, FsPolicy, Limits, Policy, ShellPolicy};

/// A temp tree with a write root, removed on drop.
struct Tree(PathBuf);

impl Tree {
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "sandbox-exec-{tag}-{}-{unique}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("work")).expect("creates the write root");
        fs::create_dir_all(root.join("outside")).expect("creates the outside dir");
        fs::write(root.join("outside/secret.txt"), b"topsecret").expect("writes the secret");
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

/// Detect the backend, or return `None` when this host cannot confine a command.
fn backend_or_skip() -> Option<Backend> {
    let path = std::env::var("PATH").unwrap_or_default();
    let backend = Backend::detect(&path, &[]);
    if backend.is_supported() {
        Some(backend)
    } else {
        None
    }
}

/// A policy that writes only inside `tree`'s `work` root.
///
/// The environment is an allowlist, so the test puts the host's `PATH` in it:
/// that is what an operator does to make the tools a command needs reachable.
/// The program and every tool it calls then resolve inside the sandbox.
fn policy(tree: &Tree) -> Policy {
    Policy {
        fs: FsPolicy {
            entries: vec![FsEntry::write(tree.work())],
            protected: Vec::new(),
        },
        shell: ShellPolicy {
            env: vec![EnvVar::new("PATH", host_path())],
            workdir: tree.work(),
        },
        limits: Limits {
            timeout_millis: 10_000,
            max_output_bytes: 1024 * 1024,
        },
        ..Policy::default()
    }
}

/// The host's `PATH`, so the confined command can resolve the tools it calls.
fn host_path() -> String {
    std::env::var("PATH").unwrap_or_else(|_| "/bin:/usr/bin".to_owned())
}

/// Run `/bin/sh -c <script>` under `policy`.
fn sh(
    backend: &Backend,
    policy: &Policy,
    scratch: &Scratch,
    script: &str,
) -> sandbox::executor::ExecOutcome {
    let request = ExecRequest {
        program: OsString::from("/bin/sh"),
        args: vec![OsString::from("-c"), OsString::from(script)],
        policy: policy.clone(),
    };
    run(backend, &request, scratch).expect("the confined command runs")
}

#[test]
fn a_write_inside_the_write_root_succeeds() {
    let Some(backend) = backend_or_skip() else {
        return;
    };
    let tree = Tree::new("write-inside");
    let scratch = Scratch::new(&tree.work()).expect("scratch");
    let policy = policy(&tree);

    let outcome = sh(
        &backend,
        &policy,
        &scratch,
        &format!(
            "echo hello > {}/new.txt && cat {}/new.txt",
            tree.work().display(),
            tree.work().display()
        ),
    );

    assert_eq!(
        outcome.code,
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&outcome.stderr)
    );
    assert_eq!(outcome.stdout, b"hello\n");
    // The write reached the host through the bind, as a real file.
    let written = fs::read_to_string(tree.work().join("new.txt")).expect("the file is on the host");
    assert_eq!(written, "hello\n");
}

#[test]
fn a_write_outside_the_write_root_is_denied_by_the_kernel() {
    let Some(backend) = backend_or_skip() else {
        return;
    };
    let tree = Tree::new("write-outside");
    let scratch = Scratch::new(&tree.work()).expect("scratch");
    let policy = policy(&tree);

    let target = tree.0.join("outside/evil.txt");
    let outcome = sh(
        &backend,
        &policy,
        &scratch,
        &format!("echo nope > {}", target.display()),
    );

    assert_ne!(outcome.code, Some(0), "the write outside must fail");
    let message = String::from_utf8_lossy(&outcome.stderr);
    assert!(
        message.contains("Read-only file system"),
        "expected a read-only failure, got: {message}"
    );
    assert!(!target.exists(), "the file must not exist on the host");
}

#[test]
fn a_denied_path_is_not_readable() {
    let Some(backend) = backend_or_skip() else {
        return;
    };
    let tree = Tree::new("deny-read");
    fs::write(tree.work().join("secret.txt"), b"hidden").expect("writes the deny target");
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

    assert_ne!(outcome.code, Some(0), "reading a denied file must fail");
    assert!(outcome.stdout.is_empty(), "nothing should have been read");
}

#[test]
fn the_host_environment_is_scrubbed() {
    let Some(backend) = backend_or_skip() else {
        return;
    };
    let tree = Tree::new("env");
    let scratch = Scratch::new(&tree.work()).expect("scratch");
    let policy = policy(&tree);

    // A variable that is present in this process must not reach the command.
    // `$HOME` is written without braces so the literal holds no `{...}` that
    // `format!` would read as an argument.
    assert!(
        std::env::var("HOME").is_ok(),
        "the test needs a host variable to prove it is not inherited"
    );
    let outcome = sh(&backend, &policy, &scratch, "echo [$HOME] [$PATH]");

    assert_eq!(outcome.code, Some(0));
    assert_eq!(
        String::from_utf8_lossy(&outcome.stdout).trim(),
        format!("[] [{}]", host_path()),
        "only the policy's environment should be present"
    );
}

#[test]
fn a_timed_out_command_is_killed_and_leaves_no_marker() {
    let Some(backend) = backend_or_skip() else {
        return;
    };
    let tree = Tree::new("timeout");
    let scratch = Scratch::new(&tree.work()).expect("scratch");
    let mut policy = policy(&tree);
    policy.limits.timeout_millis = 300;

    // The command starts a background sleep and a late write, then waits. If the
    // timeout kills the whole group, neither the sleep nor the write survives.
    let marker = tree.work().join("late.txt");
    let outcome = sh(
        &backend,
        &policy,
        &scratch,
        &format!("(sleep 2; echo late > {}) & sleep 30", marker.display()),
    );

    assert!(outcome.timed_out, "the timeout should have fired");
    // Give a surviving process a moment to write, then assert it did not.
    std::thread::sleep(Duration::from_millis(2500));
    assert!(!marker.exists(), "a killed command must not leave a marker");
}

#[test]
fn a_policy_naming_a_missing_write_root_is_a_render_error() {
    let Some(backend) = backend_or_skip() else {
        return;
    };
    let tree = Tree::new("missing-root");
    let scratch = Scratch::new(&tree.work()).expect("scratch");
    let missing = tree.work().join("does-not-exist");
    let mut policy = policy(&tree);
    policy.fs.entries = vec![FsEntry::write(missing.clone())];
    policy.shell.workdir = missing;

    let request = ExecRequest {
        program: OsString::from("/bin/true"),
        args: Vec::new(),
        policy,
    };
    let result = run(&backend, &request, &scratch);
    assert!(matches!(
        result,
        Err(ExecError::Render(RenderError::MissingWriteRoot { .. }))
    ));
}

#[test]
fn a_write_root_cannot_be_renamed() {
    let Some(backend) = backend_or_skip() else {
        return;
    };
    let tree = Tree::new("rename-root");
    let scratch = Scratch::new(&tree.work()).expect("scratch");
    let policy = policy(&tree);
    let moved = tree.work().with_extension("moved");

    let outcome = sh(
        &backend,
        &policy,
        &scratch,
        &format!("mv {} {}", tree.work().display(), moved.display()),
    );

    assert_ne!(outcome.code, Some(0), "renaming the write root must fail");
    assert!(tree.work().is_dir(), "the write root stays in place");
    assert!(
        !moved.exists(),
        "the moved name must not appear on the host"
    );
}

#[test]
fn a_traversal_out_of_the_write_root_is_denied() {
    let Some(backend) = backend_or_skip() else {
        return;
    };
    let tree = Tree::new("traversal");
    let scratch = Scratch::new(&tree.work()).expect("scratch");
    let policy = policy(&tree);

    // `..` from the write root lands on the parent, which is read-only.
    let outcome = sh(
        &backend,
        &policy,
        &scratch,
        &format!("echo nope > {}/../escaped.txt", tree.work().display()),
    );

    assert_ne!(outcome.code, Some(0), "a traversal escape must fail");
    assert!(!tree.0.join("escaped.txt").exists());
}

#[test]
fn a_symlink_out_of_the_write_root_does_not_grant_access() {
    let Some(backend) = backend_or_skip() else {
        return;
    };
    let tree = Tree::new("symlink");
    // A symlink inside the write root pointing at a file outside it. The kernel
    // follows the link, but the target is outside the bind, so it is read-only.
    let link = tree.work().join("escape");
    std::os::unix::fs::symlink(tree.0.join("outside/secret.txt"), &link)
        .expect("creates the symlink");
    let scratch = Scratch::new(&tree.work()).expect("scratch");
    let policy = policy(&tree);

    let outcome = sh(
        &backend,
        &policy,
        &scratch,
        &format!("echo nope > {}", link.display()),
    );

    assert_ne!(
        outcome.code,
        Some(0),
        "writing through the symlink must fail"
    );
    let secret = fs::read(tree.0.join("outside/secret.txt")).expect("the secret is intact");
    assert_eq!(secret, b"topsecret", "the target was not overwritten");
}

#[test]
fn output_is_capped_at_the_policy_limit() {
    let Some(backend) = backend_or_skip() else {
        return;
    };
    let tree = Tree::new("output-cap");
    let scratch = Scratch::new(&tree.work()).expect("scratch");
    let mut policy = policy(&tree);
    // A small cap, and a command that writes far more than it.
    policy.limits.max_output_bytes = 64;

    // 1000 bytes of output, well past the 64-byte cap.
    let outcome = sh(
        &backend,
        &policy,
        &scratch,
        "i=0; while [ $i -lt 1000 ]; do printf x; i=$((i+1)); done",
    );

    assert_eq!(outcome.code, Some(0), "the command still finishes");
    assert!(
        outcome.stdout.len() <= 64,
        "output must be capped, got {} bytes",
        outcome.stdout.len()
    );
}

#[test]
fn a_protected_name_that_exists_cannot_be_modified() {
    let Some(backend) = backend_or_skip() else {
        return;
    };
    let tree = Tree::new("protected");
    // A fresh `.git`, so the read-only re-bind has a mountpoint to use.
    fs::create_dir_all(tree.work().join(".git")).expect("creates .git");
    fs::write(tree.work().join(".git/config"), b"orig").expect("writes");
    let scratch = Scratch::new(&tree.work()).expect("scratch");
    let mut policy = policy(&tree);
    policy.fs.protected = vec![".git".to_owned()];

    let outcome = sh(
        &backend,
        &policy,
        &scratch,
        &format!("echo changed >> {}/.git/config", tree.work().display()),
    );

    assert_ne!(outcome.code, Some(0), "a protected name must be read-only");
    let config = fs::read(tree.work().join(".git/config")).expect("the file is intact");
    assert_eq!(config, b"orig", "the protected file was not modified");
}
