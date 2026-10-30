use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;

use super::{AllowError, Allowlist, executable};
use crate::Code;
use crate::fixture::path_var;

fn allow(entries: &[&str]) -> Allowlist {
    let entries: Vec<String> = entries.iter().map(|entry| (*entry).to_string()).collect();
    Allowlist::of(&entries).expect("the entries are usable")
}

fn sh() -> PathBuf {
    let path = path_var();
    allow(&["sh"])
        .resolve("sh", Some(&path))
        .expect("the test host has sh on its PATH")
}

fn code<T>(result: Result<T, crate::ToolError>) -> Code {
    result.err().expect("the call fails").code()
}

#[test]
fn a_relative_entry_with_a_slash_is_refused() {
    assert_eq!(
        Allowlist::of(&["bin/tool".to_string()]),
        Err(AllowError::Relative("bin/tool".to_string()))
    );
}

#[test]
fn an_empty_entry_is_refused() {
    assert_eq!(Allowlist::of(&[String::new()]), Err(AllowError::Empty));
}

#[test]
fn a_listed_name_resolves_through_the_path() {
    let found = sh();
    assert!(found.is_absolute());
}

#[test]
fn an_absolute_entry_permits_the_bare_name() {
    let path = path_var();
    let absolute = sh().to_string_lossy().into_owned();
    let allow = allow(&[absolute.as_str()]);
    assert_eq!(allow.resolve("sh", Some(&path)).expect("allowed"), sh());
}

#[test]
fn an_unlisted_program_is_refused_and_the_error_names_the_allowed_ones() {
    let error = allow(&["sh"])
        .resolve("definitely-not-installed", Some(&path_var()))
        .expect_err("refused");
    assert_eq!(error.code(), Code::ProgramNotAllowed);
    assert!(error.message().contains("sh"), "{}", error.message());
}

#[test]
fn a_listed_name_that_is_not_on_the_path_is_not_found() {
    let absent = "agent-tools-absent-program";
    assert_eq!(
        code(allow(&[absent]).resolve(absent, Some(&path_var()))),
        Code::NotFound
    );
}

#[test]
fn a_path_instead_of_a_name_is_invalid_args() {
    assert_eq!(
        code(allow(&["sh"]).resolve("/bin/sh", Some(&path_var()))),
        Code::InvalidArgs
    );
}

#[test]
fn an_empty_list_refuses_every_program() {
    let allow = Allowlist::of(&[]).expect("an empty list is usable");
    assert!(allow.is_empty());
    let error = allow.resolve("sh", Some(&path_var())).expect_err("refused");
    assert_eq!(error.code(), Code::ProgramNotAllowed);
    assert!(
        error.message().contains("allows no programs"),
        "{}",
        error.message()
    );
}

#[test]
fn a_nonempty_list_permits_something() {
    assert!(!allow(&["sh"]).is_empty());
}

#[test]
fn a_directory_is_not_a_program_even_with_every_execute_bit() {
    let dir = crate::fixture::TempDir::new();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755))
        .expect("sets the mode");
    assert_eq!(executable(dir.path()), None);
}

#[test]
fn a_file_without_an_execute_bit_is_not_a_program() {
    let dir = crate::fixture::TempDir::new();
    dir.write("plain", "x");
    let path = dir.path().join("plain");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("sets the mode");
    assert_eq!(executable(&path), None);
}

#[test]
fn a_file_with_an_execute_bit_is_the_program() {
    let dir = crate::fixture::TempDir::new();
    dir.write("tool", "x");
    let path = dir.path().join("tool");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("sets the mode");
    assert_eq!(executable(&path), Some(path.clone()));
}

#[test]
fn a_path_that_is_not_there_is_not_a_program() {
    let dir = crate::fixture::TempDir::new();
    assert_eq!(executable(&dir.path().join("absent")), None);
}
