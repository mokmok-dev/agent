use std::os::unix::fs::PermissionsExt as _;

use super::{create_from, replace};
use crate::Code;
use crate::fixture::TempDir;

fn name(attempt: u64) -> String {
    format!(".agent-tool.{}.{attempt}.tmp", std::process::id())
}

#[test]
fn a_taken_name_is_tried_again_instead_of_ending_the_write() {
    let dir = TempDir::new();
    for attempt in 0_u64..7 {
        std::fs::write(dir.path().join(name(attempt)), b"taken").expect("occupies the name");
    }
    let (file, temp) = create_from(dir.path(), 0).expect("the eighth name is free");
    assert_eq!(temp, dir.path().join(name(7)));
    assert!(file.metadata().expect("the file exists").is_file());
}

#[test]
fn eight_taken_names_are_a_failure_that_names_the_last_one() {
    let dir = TempDir::new();
    for attempt in 0_u64..8 {
        std::fs::write(dir.path().join(name(attempt)), b"taken").expect("occupies the name");
    }
    let error = create_from(dir.path(), 0).expect_err("every name is taken");
    assert_eq!(error.code(), Code::Io);
    assert!(
        error.message().contains(&name(7)),
        "the error names the last name it tried: {}",
        error.message()
    );
}

#[test]
fn a_directory_that_is_not_there_is_not_retried_eight_times() {
    let error = replace(
        std::path::Path::new("/nonexistent-agent-tools/x"),
        b"x",
        None,
    )
    .expect_err("the directory is not there");
    assert_eq!(error.code(), Code::Io);
    assert!(
        error.message().contains("No such file or directory"),
        "the failure is the one the kernel gave, not a made-up collision: {}",
        error.message()
    );
}

#[test]
fn a_directory_that_cannot_be_flushed_is_a_failure() {
    let dir = TempDir::new();
    let target = dir.path().join("note.txt");
    std::fs::write(&target, b"old").expect("writes");
    // The rename needs the directory to be writable and searchable; reading it,
    // which the flush does, needs the read bit this mode withholds.
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o300))
        .expect("sets the mode");
    let written = replace(&target, b"new", None);
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
        .expect("restores the mode");
    let error = written.expect_err("the directory cannot be read to flush it");
    assert_eq!(error.code(), Code::Io);
    assert_eq!(std::fs::read(&target).expect("reads"), b"new");
}

#[test]
fn the_bytes_land_and_the_mode_is_the_one_asked_for() {
    let dir = TempDir::new();
    let target = dir.path().join("note.txt");
    std::fs::write(&target, b"old").expect("writes");
    replace(&target, b"new", Some(0o640)).expect("replaces");
    assert_eq!(std::fs::read(&target).expect("reads"), b"new");
    let mode = std::fs::metadata(&target)
        .expect("there")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o640);
}
