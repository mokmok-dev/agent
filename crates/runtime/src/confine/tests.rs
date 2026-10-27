//! What the launcher is told, read as a value.
//!
//! The bounds themselves are tested where they are installed: `crates/agentd/tests/confine.rs`
//! runs the real launcher and watches what a process can and cannot do.

#![allow(
    clippy::expect_used,
    reason = "a test reports a broken assumption by panicking"
)]

use std::ffi::OsString;
use std::path::Path;

use super::{arch_of, bounds_from, plan, probe, search};
use crate::{Network, Settings, Tool};

fn tool() -> Tool {
    Tool {
        argv: vec!["/nix/store/coreutils/bin/cat".to_string()],
        confined: true,
        env: vec!["PATH".to_string()],
        file_bytes: 8 * 1024 * 1024,
        idempotent: true,
        max_output: 4096,
        memory_bytes: 256 * 1024 * 1024,
        name: "shell".to_string(),
        network: Network::None,
        risk: agent_core::event::Risk::Read,
        schema: "{}".to_string(),
        timeout: std::time::Duration::from_secs(5),
        write: Vec::new(),
    }
}

fn words(argv: &[OsString]) -> Vec<String> {
    argv.iter()
        .map(|word| word.to_string_lossy().into_owned())
        .collect()
}

fn planned(tool: &Tool) -> Vec<String> {
    words(&plan(
        Path::new("/nix/store/agent/bin/agent-confine"),
        tool,
        Path::new("/nix/store/coreutils/bin/cat"),
    ))
}

#[test]
fn the_launcher_is_told_every_bound_the_manifest_states() {
    assert_eq!(
        planned(&tool()),
        vec![
            "/nix/store/agent/bin/agent-confine",
            "--memory-bytes=268435456",
            "--file-bytes=8388608",
            "--network=none",
            "--",
            "/nix/store/coreutils/bin/cat",
        ]
    );
}

#[test]
fn every_declared_directory_is_named_and_nothing_else_is() {
    let mut declared = tool();
    declared.write = vec!["/var/lib/agent/out".into(), "/tmp/scratch".into()];
    let argv = planned(&declared);
    let named: Vec<&String> = argv
        .iter()
        .filter(|word| word.starts_with("--write="))
        .collect();
    assert_eq!(
        named,
        vec!["--write=/var/lib/agent/out", "--write=/tmp/scratch"]
    );
}

#[test]
fn the_network_is_shared_only_when_the_manifest_asks_for_it() {
    let mut allowed = tool();
    allowed.network = Network::Any;
    assert!(planned(&allowed).iter().any(|word| word == "--network=any"));
    assert!(planned(&tool()).iter().any(|word| word == "--network=none"));
}

#[test]
fn the_command_line_reads_back_as_the_same_bounds() {
    let mut declared = tool();
    declared.network = Network::Any;
    declared.write = vec!["/var/lib/agent/out".into()];
    let argv = plan(
        Path::new("/bin/agent-confine"),
        &declared,
        Path::new("/nix/store/coreutils/bin/cat"),
    );
    let (bounds, program) = bounds_from(&argv[1..]).expect("the launcher's own line");
    assert_eq!(bounds.memory_bytes, declared.memory_bytes);
    assert_eq!(bounds.file_bytes, declared.file_bytes);
    assert_eq!(bounds.network, Network::Any);
    assert_eq!(bounds.write, declared.write);
    assert_eq!(
        words(&program),
        vec!["/nix/store/coreutils/bin/cat"],
        "the program and its arguments come after the separator"
    );
}

#[test]
fn an_option_the_daemon_did_not_write_is_refused() {
    let argv = vec![
        OsString::from("--memory-bytes=1"),
        OsString::from("--file-bytes=1"),
        OsString::from("--network=none"),
        OsString::from("--unshare-all"),
        OsString::from("--"),
        OsString::from("/bin/true"),
    ];
    let error = bounds_from(&argv).expect_err("an unknown option is not a bound");
    assert!(error.contains("--unshare-all"), "{error}");
}

#[test]
fn a_command_line_that_names_no_program_is_refused() {
    let argv = vec![
        OsString::from("--memory-bytes=1"),
        OsString::from("--file-bytes=1"),
        OsString::from("--network=none"),
    ];
    let error = bounds_from(&argv).expect_err("nothing to become");
    assert!(error.contains("no program"), "{error}");
}

#[test]
fn a_bound_that_is_not_a_number_is_refused() {
    let argv = vec![
        OsString::from("--memory-bytes=lots"),
        OsString::from("--file-bytes=1"),
        OsString::from("--network=none"),
        OsString::from("--"),
        OsString::from("/bin/true"),
    ];
    let error = bounds_from(&argv).expect_err("lots is not a number of bytes");
    assert!(error.contains("lots"), "{error}");
}

/// A directory that is removed when the test ends.
struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("agent-confine-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("creates the scratch directory");
        Self(dir)
    }

    fn file(
        &self,
        relative: &str,
        mode: u32,
    ) -> std::path::PathBuf {
        let path = self.0.join(relative);
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("creates a directory");
        std::fs::write(&path, "").expect("writes a file");
        std::fs::set_permissions(
            &path,
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(mode),
        )
        .expect("sets permissions");
        path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn a_directory_or_a_file_that_cannot_run_is_not_the_program() {
    let scratch = Scratch::new("shadowed");
    // A PATH is searched in order, and the first entry with the program's name
    // wins. A directory of that name and a file the daemon cannot execute both
    // appear earlier than the one that runs, which is the case that decides
    // whether the tool starts at all.
    let unexecutable = scratch.file("file/tool", 0o644);
    let directory = scratch.0.join("directory/tool");
    std::fs::create_dir_all(&directory).expect("creates a directory");
    let runnable = scratch.file("real/tool", 0o755);
    let path = format!(
        "{}:{}:{}",
        unexecutable.parent().expect("a parent").display(),
        directory.parent().expect("a parent").display(),
        runnable.parent().expect("a parent").display()
    );

    assert_eq!(
        search("tool", std::ffi::OsStr::new(&path)),
        Some(runnable),
        "the third entry is the only one that could run"
    );
    assert_eq!(search("tool", std::ffi::OsStr::new("")), None);
}

#[test]
fn a_confined_tool_whose_launcher_is_not_there_is_refused_by_name() {
    // No launcher beside the running binary and none named: a tool that cannot be
    // bounded must not be started anyway.
    let settings = Settings {
        approval_grace: core::time::Duration::from_secs(600),
        directive: "be careful".to_string(),
        directive_version: "test".to_string(),
        max_output_tokens: 64,
        model: "test-model".to_string(),
        tools: vec![tool()],
    };
    let said = probe(&settings).expect_err("there is no launcher here");
    assert!(said.contains("is not beside"), "{said}");
}

#[test]
fn the_filter_is_built_for_the_machine_it_runs_on() {
    assert_eq!(
        arch_of("aarch64"),
        seccompiler::TargetArch::aarch64,
        "an arm64 build filters arm64 syscalls"
    );
    assert_eq!(arch_of("riscv64"), seccompiler::TargetArch::riscv64);
    assert_eq!(arch_of("x86_64"), seccompiler::TargetArch::x86_64);
    assert_eq!(
        arch_of("something else"),
        seccompiler::TargetArch::x86_64,
        "an unknown name falls back to the machine this build was tested on"
    );
}
