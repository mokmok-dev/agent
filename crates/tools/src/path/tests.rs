use std::os::unix::fs::symlink;

use super::Workspace;
use crate::Code;
use crate::fixture::TempDir;

fn code<T>(result: Result<T, crate::ToolError>) -> Code {
    result.err().expect("the call fails").code()
}

#[test]
fn an_absolute_path_is_refused() {
    let dir = TempDir::new();
    assert_eq!(
        code(dir.workspace().existing("/etc/passwd")),
        Code::OutsideWorkspace
    );
}

#[test]
fn a_parent_component_is_refused() {
    let dir = TempDir::new();
    let workspace = dir.workspace();
    assert_eq!(code(workspace.existing("..")), Code::OutsideWorkspace);
    assert_eq!(
        code(workspace.existing("inside/../../elsewhere")),
        Code::OutsideWorkspace
    );
    assert_eq!(
        code(workspace.writable("../elsewhere")),
        Code::OutsideWorkspace
    );
}

#[test]
fn a_symlink_that_leaves_the_workspace_is_refused() {
    let outside = TempDir::new();
    outside.write("secret", "x");
    let dir = TempDir::new();
    symlink(outside.path(), dir.path().join("link")).expect("creates the link");
    let workspace = dir.workspace();
    assert_eq!(
        code(workspace.existing("link/secret")),
        Code::OutsideWorkspace
    );
    assert_eq!(code(workspace.writable("link/new")), Code::OutsideWorkspace);
}

#[test]
fn a_symlink_inside_the_workspace_is_followed() {
    let dir = TempDir::new();
    dir.write("real/file", "x");
    symlink(dir.path().join("real"), dir.path().join("link")).expect("creates the link");
    assert_eq!(
        dir.workspace().existing("link/file").expect("resolves"),
        dir.path().join("real/file")
    );
}

#[test]
fn a_missing_path_is_not_found() {
    let dir = TempDir::new();
    assert_eq!(code(dir.workspace().existing("absent")), Code::NotFound);
}

#[test]
fn writable_names_a_file_that_does_not_exist_yet() {
    let dir = TempDir::new();
    dir.write("present/file", "x");
    assert_eq!(
        dir.workspace().writable("present/new").expect("resolves"),
        dir.path().join("present/new")
    );
    assert_eq!(
        dir.workspace().writable("absent/new").expect("resolves"),
        dir.path().join("absent/new")
    );
}

#[test]
fn a_path_is_reported_relative_to_the_root() {
    let dir = TempDir::new();
    dir.write("a/b", "x");
    let workspace = Workspace::open(dir.path()).expect("opens");
    assert_eq!(workspace.relative(&dir.path().join("a/b")), "a/b");
    assert_eq!(workspace.relative(&dir.path().join("a")), "a");
}
