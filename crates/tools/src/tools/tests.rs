use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::time::{Duration, Instant};

use super::{edit_file, list_dir, read_file, run, search, write_file};
use crate::fixture::{self, TempDir};
use crate::{Allowlist, Code, Tool};

fn empty() -> Allowlist {
    Allowlist::default()
}

fn shell() -> Allowlist {
    Allowlist::of(&["sh".to_string()]).expect("sh is usable")
}

fn code<T>(result: Result<T, crate::ToolError>) -> Code {
    result.err().expect("the call fails").code()
}

#[test]
fn a_whole_file_is_read_with_its_line_numbers() {
    let dir = TempDir::new();
    dir.write("note.txt", "one\ntwo\nthree\n");
    let workspace = dir.workspace();
    let allow = empty();
    let ctx = fixture::ctx(&workspace, &allow, dir.path());
    let out = read_file::ReadFile::run(
        read_file::Args {
            path: "note.txt".to_string(),
            start_line: None,
            end_line: None,
        },
        &ctx,
    )
    .expect("reads");
    assert_eq!(out.start_line, 1);
    assert_eq!(out.end_line, 3);
    assert_eq!(out.lines, 3);
    assert_eq!(out.content, "one\ntwo\nthree\n");
    assert!(!out.truncated);
}

#[test]
fn a_window_is_one_based_and_inclusive() {
    let dir = TempDir::new();
    dir.write("note.txt", "one\ntwo\nthree\n");
    let workspace = dir.workspace();
    let allow = empty();
    let ctx = fixture::ctx(&workspace, &allow, dir.path());
    let out = read_file::ReadFile::run(
        read_file::Args {
            path: "note.txt".to_string(),
            start_line: Some(2),
            end_line: Some(2),
        },
        &ctx,
    )
    .expect("reads");
    assert_eq!(out.start_line, 2);
    assert_eq!(out.end_line, 2);
    assert_eq!(out.content, "two\n");
}

#[test]
fn an_empty_file_reads_as_no_lines() {
    let dir = TempDir::new();
    dir.write("empty.txt", "");
    let workspace = dir.workspace();
    let allow = empty();
    let ctx = fixture::ctx(&workspace, &allow, dir.path());
    let out = read_file::ReadFile::run(
        read_file::Args {
            path: "empty.txt".to_string(),
            start_line: None,
            end_line: None,
        },
        &ctx,
    )
    .expect("reads");
    assert_eq!(out.lines, 0);
    assert_eq!(out.content, "");
    assert!(!out.truncated);
}

#[test]
fn a_window_past_the_end_is_not_found() {
    let dir = TempDir::new();
    dir.write("note.txt", "one\n");
    let workspace = dir.workspace();
    let allow = empty();
    let ctx = fixture::ctx(&workspace, &allow, dir.path());
    assert_eq!(
        code(read_file::ReadFile::run(
            read_file::Args {
                path: "note.txt".to_string(),
                start_line: Some(9),
                end_line: None,
            },
            &ctx,
        )),
        Code::NotFound
    );
}

#[test]
fn a_file_that_is_not_utf8_is_not_text() {
    let dir = TempDir::new();
    dir.write_bytes("bad.txt", &[b'a', 0xff, b'b']);
    let workspace = dir.workspace();
    let allow = empty();
    let ctx = fixture::ctx(&workspace, &allow, dir.path());
    assert_eq!(
        code(read_file::ReadFile::run(
            read_file::Args {
                path: "bad.txt".to_string(),
                start_line: None,
                end_line: None,
            },
            &ctx,
        )),
        Code::NotText
    );
}

#[test]
fn a_last_line_without_a_newline_counts() {
    let dir = TempDir::new();
    dir.write("note.txt", "one\ntwo");
    let workspace = dir.workspace();
    let allow = empty();
    let ctx = fixture::ctx(&workspace, &allow, dir.path());
    let out = read_file::ReadFile::run(
        read_file::Args {
            path: "note.txt".to_string(),
            start_line: None,
            end_line: None,
        },
        &ctx,
    )
    .expect("reads");
    assert_eq!(out.lines, 2);
    assert_eq!(out.content, "one\ntwo");
}

#[test]
fn a_long_file_stops_at_the_content_cap_and_says_so() {
    let dir = TempDir::new();
    dir.write("long.txt", &"line\n".repeat(20_000));
    let workspace = dir.workspace();
    let allow = empty();
    let ctx = fixture::ctx(&workspace, &allow, dir.path());
    let out = read_file::ReadFile::run(
        read_file::Args {
            path: "long.txt".to_string(),
            start_line: None,
            end_line: None,
        },
        &ctx,
    )
    .expect("reads");
    assert!(out.truncated);
    assert!(out.content.len() <= 48 * 1024);
    assert_eq!(out.lines, out.content.matches('\n').count());
}

#[test]
fn a_result_that_escapes_large_still_fits_its_event_cap() {
    let dir = TempDir::new();
    dir.write("quotes.txt", &"\"".repeat(48 * 1024));
    let workspace = dir.workspace();
    let allow = empty();
    let ctx = fixture::ctx(&workspace, &allow, dir.path());
    let out = read_file::ReadFile::run(
        read_file::Args {
            path: "quotes.txt".to_string(),
            start_line: None,
            end_line: None,
        },
        &ctx,
    )
    .expect("reads");
    let answer = serde_json::to_vec(&out).expect("serializes");
    assert!(answer.len() <= 64 * 1024);
    assert!(out.truncated);
}

#[test]
fn a_listing_is_sorted_and_one_level_deep_by_default() {
    let dir = TempDir::new();
    dir.write("b.txt", "");
    dir.write("a.txt", "");
    dir.write("sub/c.txt", "");
    let workspace = dir.workspace();
    let allow = empty();
    let ctx = fixture::ctx(&workspace, &allow, dir.path());
    let out = list_dir::ListDir::run(
        list_dir::Args {
            path: None,
            depth: None,
        },
        &ctx,
    )
    .expect("lists");
    let paths: Vec<&str> = out
        .entries
        .iter()
        .map(|entry| entry.path.as_str())
        .collect();
    assert_eq!(paths, ["a.txt", "b.txt", "sub"]);
}

#[test]
fn a_listing_of_depth_two_includes_the_subtree() {
    let dir = TempDir::new();
    dir.write("sub/c.txt", "");
    let workspace = dir.workspace();
    let allow = empty();
    let ctx = fixture::ctx(&workspace, &allow, dir.path());
    let out = list_dir::ListDir::run(
        list_dir::Args {
            path: Some(".".to_string()),
            depth: Some(2),
        },
        &ctx,
    )
    .expect("lists");
    let paths: Vec<&str> = out
        .entries
        .iter()
        .map(|entry| entry.path.as_str())
        .collect();
    assert_eq!(paths, ["sub", "sub/c.txt"]);
}

#[test]
fn a_symlink_is_reported_and_never_followed() {
    let outside = TempDir::new();
    outside.write("secret.txt", "x");
    let dir = TempDir::new();
    symlink(outside.path(), dir.path().join("link")).expect("creates the link");
    let workspace = dir.workspace();
    let allow = empty();
    let ctx = fixture::ctx(&workspace, &allow, dir.path());
    let out = list_dir::ListDir::run(
        list_dir::Args {
            path: None,
            depth: Some(3),
        },
        &ctx,
    )
    .expect("lists");
    assert_eq!(out.entries.len(), 1);
    assert_eq!(out.entries[0].path, "link");
    assert!(matches!(out.entries[0].kind, list_dir::Kind::Symlink));
}

#[test]
fn git_is_shown_and_not_recursed_into() {
    let dir = TempDir::new();
    dir.write(".git/config", "");
    let workspace = dir.workspace();
    let allow = empty();
    let ctx = fixture::ctx(&workspace, &allow, dir.path());
    let out = list_dir::ListDir::run(
        list_dir::Args {
            path: None,
            depth: Some(3),
        },
        &ctx,
    )
    .expect("lists");
    let paths: Vec<&str> = out
        .entries
        .iter()
        .map(|entry| entry.path.as_str())
        .collect();
    assert_eq!(paths, [".git"]);
}

#[test]
fn a_listing_stops_at_its_entry_and_byte_caps() {
    let dir = TempDir::new();
    for number in 0..1100 {
        dir.write(&format!("file{number:04}.txt"), "");
    }
    let workspace = dir.workspace();
    let allow = empty();
    let ctx = fixture::ctx(&workspace, &allow, dir.path());
    let out = list_dir::ListDir::run(
        list_dir::Args {
            path: None,
            depth: Some(1),
        },
        &ctx,
    )
    .expect("lists");
    assert!(out.entries.len() <= 1000);
    assert!(out.truncated);
    let answer = serde_json::to_vec(&out).expect("serializes");
    assert!(answer.len() <= 24 * 1024);
}

#[test]
fn listing_a_file_is_not_a_directory() {
    let dir = TempDir::new();
    dir.write("a.txt", "");
    let workspace = dir.workspace();
    let allow = empty();
    let ctx = fixture::ctx(&workspace, &allow, dir.path());
    assert_eq!(
        code(list_dir::ListDir::run(
            list_dir::Args {
                path: Some("a.txt".to_string()),
                depth: None,
            },
            &ctx,
        )),
        Code::NotAFile
    );
}

#[test]
fn a_depth_outside_one_to_three_is_invalid_args() {
    let dir = TempDir::new();
    let workspace = dir.workspace();
    let allow = empty();
    let ctx = fixture::ctx(&workspace, &allow, dir.path());
    for depth in [0, 4] {
        assert_eq!(
            code(list_dir::ListDir::run(
                list_dir::Args {
                    path: None,
                    depth: Some(depth),
                },
                &ctx,
            )),
            Code::InvalidArgs
        );
    }
}

#[test]
fn a_search_reports_path_line_and_column() {
    let dir = TempDir::new();
    dir.write("a.txt", "alpha\nbeta alpha\n");
    let workspace = dir.workspace();
    let allow = empty();
    let ctx = fixture::ctx(&workspace, &allow, dir.path());
    let out = search::Search::run(
        search::Args {
            pattern: "alpha".to_string(),
            path: None,
            glob: None,
            case_sensitive: None,
            max_results: None,
        },
        &ctx,
    )
    .expect("searches");
    let found: Vec<(&str, usize, usize)> = out
        .matches
        .iter()
        .map(|found| (found.path.as_str(), found.line, found.column))
        .collect();
    assert_eq!(found, [("a.txt", 1, 1), ("a.txt", 2, 6)]);
    assert_eq!(out.matches[1].text, "beta alpha");
}

#[test]
fn a_glob_limits_the_files_searched() {
    let dir = TempDir::new();
    dir.write("a.rs", "needle");
    dir.write("b.txt", "needle");
    let workspace = dir.workspace();
    let allow = empty();
    let ctx = fixture::ctx(&workspace, &allow, dir.path());
    let out = search::Search::run(
        search::Args {
            pattern: "needle".to_string(),
            path: None,
            glob: Some("*.rs".to_string()),
            case_sensitive: None,
            max_results: None,
        },
        &ctx,
    )
    .expect("searches");
    assert_eq!(out.matches.len(), 1);
    assert_eq!(out.matches[0].path, "a.rs");
}

#[test]
fn a_case_insensitive_search_finds_both_spellings() {
    let dir = TempDir::new();
    dir.write("a.txt", "Needle\nneedle\n");
    let workspace = dir.workspace();
    let allow = empty();
    let ctx = fixture::ctx(&workspace, &allow, dir.path());
    let out = search::Search::run(
        search::Args {
            pattern: "needle".to_string(),
            path: None,
            glob: None,
            case_sensitive: Some(false),
            max_results: None,
        },
        &ctx,
    )
    .expect("searches");
    assert_eq!(out.matches.len(), 2);
}

#[test]
fn a_binary_file_and_a_git_directory_are_skipped() {
    let dir = TempDir::new();
    dir.write_bytes("binary.dat", b"needle\0needle");
    dir.write(".git/config", "needle");
    let workspace = dir.workspace();
    let allow = empty();
    let ctx = fixture::ctx(&workspace, &allow, dir.path());
    let out = search::Search::run(
        search::Args {
            pattern: "needle".to_string(),
            path: None,
            glob: None,
            case_sensitive: None,
            max_results: None,
        },
        &ctx,
    )
    .expect("searches");
    assert!(out.matches.is_empty());
}

#[test]
fn a_search_stops_at_two_hundred_matches() {
    let dir = TempDir::new();
    dir.write("many.txt", &"needle\n".repeat(250));
    let workspace = dir.workspace();
    let allow = empty();
    let ctx = fixture::ctx(&workspace, &allow, dir.path());
    let out = search::Search::run(
        search::Args {
            pattern: "needle".to_string(),
            path: None,
            glob: None,
            case_sensitive: None,
            max_results: Some(1000),
        },
        &ctx,
    )
    .expect("searches");
    assert_eq!(out.matches.len(), 200);
    assert!(out.truncated);
}

#[test]
fn an_invalid_pattern_is_invalid_args() {
    let dir = TempDir::new();
    let workspace = dir.workspace();
    let allow = empty();
    let ctx = fixture::ctx(&workspace, &allow, dir.path());
    assert_eq!(
        code(search::Search::run(
            search::Args {
                pattern: "(".to_string(),
                path: None,
                glob: None,
                case_sensitive: None,
                max_results: None,
            },
            &ctx,
        )),
        Code::InvalidArgs
    );
}

#[test]
fn writing_creates_parents_and_leaves_no_temporary_file() {
    let dir = TempDir::new();
    let workspace = dir.workspace();
    let allow = empty();
    let ctx = fixture::ctx(&workspace, &allow, dir.path());
    let out = write_file::WriteFile::run(
        write_file::Args {
            path: "deep/er/note.txt".to_string(),
            content: "hello".to_string(),
        },
        &ctx,
    )
    .expect("writes");
    assert_eq!(out.bytes, 5);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("deep/er/note.txt")).expect("reads"),
        "hello"
    );
    let names: Vec<String> = std::fs::read_dir(dir.path().join("deep/er"))
        .expect("lists")
        .map(|item| {
            item.expect("an entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert_eq!(names, ["note.txt"]);
}

#[test]
fn writing_the_same_bytes_twice_is_one_file() {
    let dir = TempDir::new();
    dir.write("note.txt", "old");
    let workspace = dir.workspace();
    let allow = empty();
    let ctx = fixture::ctx(&workspace, &allow, dir.path());
    let args = || write_file::Args {
        path: "note.txt".to_string(),
        content: "new".to_string(),
    };
    write_file::WriteFile::run(args(), &ctx).expect("writes");
    write_file::WriteFile::run(args(), &ctx).expect("writes again");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("note.txt")).expect("reads"),
        "new"
    );
}

#[test]
fn content_over_the_limit_is_too_large() {
    let dir = TempDir::new();
    let workspace = dir.workspace();
    let allow = empty();
    let ctx = fixture::ctx(&workspace, &allow, dir.path());
    assert_eq!(
        code(write_file::WriteFile::run(
            write_file::Args {
                path: "big.txt".to_string(),
                content: "x".repeat(16 * 1024 * 1024 + 1),
            },
            &ctx,
        )),
        Code::TooLarge
    );
}

#[test]
fn writing_over_a_directory_is_not_a_file() {
    let dir = TempDir::new();
    dir.write("taken/file", "");
    let workspace = dir.workspace();
    let allow = empty();
    let ctx = fixture::ctx(&workspace, &allow, dir.path());
    assert_eq!(
        code(write_file::WriteFile::run(
            write_file::Args {
                path: "taken".to_string(),
                content: "x".to_string(),
            },
            &ctx,
        )),
        Code::NotAFile
    );
}

#[test]
fn writing_outside_the_workspace_is_refused() {
    let dir = TempDir::new();
    let workspace = dir.workspace();
    let allow = empty();
    let ctx = fixture::ctx(&workspace, &allow, dir.path());
    assert_eq!(
        code(write_file::WriteFile::run(
            write_file::Args {
                path: "../escaped.txt".to_string(),
                content: "x".to_string(),
            },
            &ctx,
        )),
        Code::OutsideWorkspace
    );
}

fn edit(
    ctx: &crate::Ctx<'_>,
    old: &str,
    new: &str,
    all: Option<bool>,
) -> Result<edit_file::Output, crate::ToolError> {
    edit_file::EditFile::run(
        edit_file::Args {
            path: "note.txt".to_string(),
            old: old.to_string(),
            new: new.to_string(),
            all,
        },
        ctx,
    )
}

#[test]
fn an_edit_replaces_the_one_occurrence() {
    let dir = TempDir::new();
    dir.write("note.txt", "alpha beta\n");
    let workspace = dir.workspace();
    let allow = empty();
    let ctx = fixture::ctx(&workspace, &allow, dir.path());
    let out = edit(&ctx, "beta", "gamma", None).expect("edits");
    assert_eq!(out.replacements, 1);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("note.txt")).expect("reads"),
        "alpha gamma\n"
    );
}

#[test]
fn a_missing_old_text_is_not_found() {
    let dir = TempDir::new();
    dir.write("note.txt", "alpha\n");
    let workspace = dir.workspace();
    let allow = empty();
    let ctx = fixture::ctx(&workspace, &allow, dir.path());
    assert_eq!(code(edit(&ctx, "beta", "gamma", None)), Code::NotFound);
}

#[test]
fn two_occurrences_without_all_is_ambiguous() {
    let dir = TempDir::new();
    dir.write("note.txt", "alpha alpha\n");
    let workspace = dir.workspace();
    let allow = empty();
    let ctx = fixture::ctx(&workspace, &allow, dir.path());
    assert_eq!(code(edit(&ctx, "alpha", "beta", None)), Code::Ambiguous);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("note.txt")).expect("reads"),
        "alpha alpha\n"
    );
}

#[test]
fn all_replaces_every_occurrence() {
    let dir = TempDir::new();
    dir.write("note.txt", "alpha alpha\n");
    let workspace = dir.workspace();
    let allow = empty();
    let ctx = fixture::ctx(&workspace, &allow, dir.path());
    let out = edit(&ctx, "alpha", "beta", Some(true)).expect("edits");
    assert_eq!(out.replacements, 2);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("note.txt")).expect("reads"),
        "beta beta\n"
    );
}

#[test]
fn an_empty_old_text_is_invalid_args() {
    let dir = TempDir::new();
    dir.write("note.txt", "alpha\n");
    let workspace = dir.workspace();
    let allow = empty();
    let ctx = fixture::ctx(&workspace, &allow, dir.path());
    assert_eq!(code(edit(&ctx, "", "beta", None)), Code::InvalidArgs);
}

#[test]
fn an_edit_preserves_the_mode() {
    let dir = TempDir::new();
    dir.write("note.txt", "alpha\n");
    std::fs::set_permissions(
        dir.path().join("note.txt"),
        std::fs::Permissions::from_mode(0o750),
    )
    .expect("sets the mode");
    let workspace = dir.workspace();
    let allow = empty();
    let ctx = fixture::ctx(&workspace, &allow, dir.path());
    edit(&ctx, "alpha", "beta", None).expect("edits");
    let mode = std::fs::metadata(dir.path().join("note.txt"))
        .expect("reads the metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o750);
}

#[test]
fn a_second_identical_edit_finds_nothing() {
    let dir = TempDir::new();
    dir.write("note.txt", "alpha\n");
    let workspace = dir.workspace();
    let allow = empty();
    let ctx = fixture::ctx(&workspace, &allow, dir.path());
    edit(&ctx, "alpha", "beta", None).expect("edits");
    assert_eq!(code(edit(&ctx, "alpha", "beta", None)), Code::NotFound);
}

fn run_sh(
    ctx: &crate::Ctx<'_>,
    script: &str,
    extra: &[&str],
) -> Result<run::Output, crate::ToolError> {
    let mut args = vec!["-c".to_string(), script.to_string(), "sh".to_string()];
    args.extend(extra.iter().map(|arg| (*arg).to_string()));
    run::execute(
        run::Args {
            program: "sh".to_string(),
            arguments: Some(args),
            stdin: None,
        },
        ctx,
        Duration::from_secs(10),
    )
}

#[test]
fn a_program_runs_in_the_workspace_and_its_status_is_data() {
    let dir = TempDir::new();
    let workspace = dir.workspace();
    let allow = shell();
    let ctx = fixture::ctx(&workspace, &allow, dir.path());
    let out = run_sh(&ctx, "pwd; echo err >&2; exit 3", &[]).expect("runs");
    assert_eq!(out.status, 3);
    assert_eq!(out.stdout.trim(), workspace.root().to_string_lossy());
    assert_eq!(out.stderr.trim(), "err");
    assert!(!out.stdout_truncated);
}

#[test]
fn a_noisy_program_is_trimmed_to_the_event_cap() {
    let dir = TempDir::new();
    let workspace = dir.workspace();
    let allow = shell();
    let ctx = fixture::ctx(&workspace, &allow, dir.path());
    let script = "yes | head -c 100000; yes | head -c 100000 >&2";
    let out = run_sh(&ctx, script, &[]).expect("runs");
    assert!(out.stdout_truncated);
    assert!(out.stderr_truncated);
    let answer = serde_json::to_vec(&out).expect("serializes");
    assert!(answer.len() <= 64 * 1024);
}

#[test]
fn an_argument_arrives_as_one_word() {
    let dir = TempDir::new();
    let workspace = dir.workspace();
    let allow = shell();
    let ctx = fixture::ctx(&workspace, &allow, dir.path());
    let out = run_sh(&ctx, "printf '%s\\n' \"$@\"", &["a b", "$HOME"]).expect("runs");
    assert_eq!(out.stdout, "a b\n$HOME\n");
}

#[test]
fn standard_input_is_written() {
    let dir = TempDir::new();
    let workspace = dir.workspace();
    let allow = shell();
    let ctx = fixture::ctx(&workspace, &allow, dir.path());
    let out = run::execute(
        run::Args {
            program: "sh".to_string(),
            arguments: Some(vec!["-c".to_string(), "cat".to_string()]),
            stdin: Some("hello".to_string()),
        },
        &ctx,
        Duration::from_secs(10),
    )
    .expect("runs");
    assert_eq!(out.stdout, "hello");
}

#[test]
fn an_unlisted_program_is_refused() {
    let dir = TempDir::new();
    let workspace = dir.workspace();
    let allow = shell();
    let ctx = fixture::ctx(&workspace, &allow, dir.path());
    let error = run::execute(
        run::Args {
            program: "ls".to_string(),
            arguments: None,
            stdin: None,
        },
        &ctx,
        Duration::from_secs(10),
    )
    .expect_err("refused");
    assert_eq!(error.code(), Code::ProgramNotAllowed);
    assert!(error.message().contains("sh"), "{}", error.message());
}

#[test]
fn a_path_instead_of_a_program_name_is_invalid_args() {
    let dir = TempDir::new();
    let workspace = dir.workspace();
    let allow = shell();
    let ctx = fixture::ctx(&workspace, &allow, dir.path());
    assert_eq!(
        code(run::execute(
            run::Args {
                program: "/bin/sh".to_string(),
                arguments: None,
                stdin: None,
            },
            &ctx,
            Duration::from_secs(10),
        )),
        Code::InvalidArgs
    );
}

#[test]
fn a_timeout_stops_the_whole_program() {
    let dir = TempDir::new();
    let workspace = dir.workspace();
    let allow = shell();
    let ctx = fixture::ctx(&workspace, &allow, dir.path());
    let started = Instant::now();
    let out = run::execute(
        run::Args {
            program: "sh".to_string(),
            arguments: Some(vec!["-c".to_string(), "sleep 10".to_string()]),
            stdin: None,
        },
        &ctx,
        Duration::from_millis(100),
    )
    .expect("runs");
    assert!(out.timed_out);
    assert!(started.elapsed() < Duration::from_secs(5));
}
