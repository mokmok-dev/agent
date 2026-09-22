//! Guards the dev shell's confinement backend.
//!
//! A Linux host with no `bubblewrap` cannot give a session a private network
//! namespace, and the sandbox fails closed rather than downgrading to a weaker
//! form (see `docs/egress.md`). The dev shell is how a Linux developer gets it,
//! so this asserts the flake still ships it: the sandbox tests that need a
//! namespace skip when `bwrap` is absent, and a skip is not a failure.

use std::path::PathBuf;

/// A file of the repository, relative to the workspace root.
fn workspace_file(relative: &str) -> std::io::Result<String> {
    std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join(relative),
    )
}

#[test]
fn the_dev_shell_ships_the_confinement_backend() {
    let flake = workspace_file("flake.nix").expect("the flake should be readable");
    let dev_shell = flake
        .split("devShells.default")
        .nth(1)
        .expect("the dev shell")
        .split("pre-commit.settings")
        .next()
        .expect("the dev shell block");

    assert!(
        dev_shell.contains("bubblewrap"),
        "the dev shell should ship bubblewrap, or a Linux session that needs a \
         private network namespace cannot start"
    );
}
