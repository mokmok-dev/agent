//! Guards the reconciliation ledger against going stale.
//!
//! The ledger is the record of what is built and what is deliberately not, and
//! it is easy for it to fall behind the code: its phase 2 row still read
//! "Missing (bubblewrap)" after `bubblewrap` had shipped in the dev shell. These
//! assertions are the mechanical half of that record, so a phase cannot be
//! reopened by changing code without changing the ledger that says it is done.
//!
//! The helpers use `expect` like the other integration tests; the workspace
//! `allow-*-in-tests` clippy configuration does not see integration test files,
//! so it is replicated here.
#![expect(
    clippy::expect_used,
    reason = "integration tests use expect for setup and assertions"
)]

use std::path::PathBuf;

/// A file of the repository, relative to the workspace root.
fn workspace_file(relative: &str) -> std::io::Result<String> {
    std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join(relative),
    )
}

/// The row of a markdown table whose first cell is exactly `cell`.
fn row<'a>(
    table: &'a str,
    cell: &str,
) -> &'a str {
    let marker = format!("| {cell} |");
    table
        .lines()
        .find(|line| line.starts_with(&marker))
        .expect("a backlog row for the phase")
}

/// The last cell of a markdown table row, which is where a status lives.
fn status_cell(row: &str) -> &str {
    row.trim_matches('|')
        .split('|')
        .next_back()
        .map(str::trim)
        .unwrap_or_default()
}

#[test]
fn the_ledger_marks_the_built_phases_done_with_their_commits() {
    let ledger = workspace_file("docs/reconciliation.md").expect("the ledger should be readable");
    let backlog = ledger
        .split("## Adopted backlog")
        .nth(1)
        .expect("the backlog section");
    let backlog = backlog
        .split("### Phase 2 detail")
        .next()
        .expect("the backlog table");

    for (phase, scope) in [("2", "bubblewrap"), ("3", "approver"), ("4", "patcher")] {
        let status = status_cell(row(backlog, phase));
        assert!(
            status.starts_with("Done (`"),
            "phase {phase} ({scope}) should be Done with its closing commits: {status}"
        );
        assert!(
            !status.contains("Open"),
            "phase {phase} ({scope}) should no longer be Open: {status}"
        );
    }

    // A row that still says a capability is missing would contradict the code.
    let matrix = ledger
        .split("## Component matrix")
        .nth(1)
        .expect("the component matrix")
        .split("## Architectural conflicts")
        .next()
        .expect("the matrix table");
    let missing: Vec<&str> = matrix
        .lines()
        .filter(|line| line.starts_with('|') && line.contains("| Missing |"))
        .collect();
    assert!(
        missing.is_empty(),
        "the component matrix still lists missing capabilities: {missing:#?}"
    );
}

#[test]
fn the_dev_shell_ships_the_confinement_backend_the_ledger_claims() {
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
        "the dev shell should ship bubblewrap, which phase 2 is closed on"
    );
}
