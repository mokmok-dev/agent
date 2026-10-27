//! What a command prints, and how a store failure becomes the shared error.
//!
//! Every shape here is one the interface spec fixes: text for a person, one
//! object with a `schema` field for a script. Nothing in this module prints, so
//! its output is asserted as a string rather than captured from a terminal.

use agent_core::ids::BranchId;
use agent_store::{StoreError, VerifyReport};
use serde_json::json;

use crate::exit::{Class, Exit, Failure};
use crate::status;

/// The version line, in the shape `agentctl version` prints.
#[must_use]
pub fn version_text() -> String {
    format!("agentctl {}", env!("CARGO_PKG_VERSION"))
}

/// The version object.
#[must_use]
pub fn version_json() -> String {
    json!({
        "schema": "agentctl.version/1",
        "version": env!("CARGO_PKG_VERSION"),
    })
    .to_string()
}

/// What a verification found, in a sentence.
#[must_use]
pub fn verify_text(report: &VerifyReport) -> String {
    format!(
        "ok: {} events across {} branches",
        report.events, report.branches
    )
}

/// What a verification found, as one object.
#[must_use]
pub fn verify_json(report: &VerifyReport) -> String {
    json!({
        "schema": "agentctl.verify/1",
        "ok": true,
        "events": report.events,
        "branches": report.branches,
    })
    .to_string()
}

/// The branch a fork created, in a sentence.
#[must_use]
pub fn fork_text(
    branch: BranchId,
    depth: u64,
) -> String {
    format!("forked {branch} at depth {depth}")
}

/// The branch a fork created, as one object.
#[must_use]
pub fn fork_json(
    branch: BranchId,
    depth: u64,
) -> String {
    json!({
        "schema": "agentctl.fork/1",
        "branch": branch.to_string(),
        "depth": depth,
    })
    .to_string()
}

/// The failure a store error becomes.
///
/// Storage that could not be read is a storage failure an operator fixes by
/// making room or fixing a path. Data that no longer matches what the writer
/// promised is a corrupt store, and the remedy is a restore, not a repair.
#[must_use]
pub fn store_failure(error: &StoreError) -> Failure {
    if error.is_corrupt() {
        Failure {
            exit: Exit::Corrupt,
            class: Class::Corrupt,
            message: error.to_string(),
            hint: Some(
                "stop the service, restore the last backup, then run `agentctl verify`".to_string(),
            ),
        }
    } else {
        Failure {
            exit: Exit::Storage,
            class: Class::Storage,
            message: error.to_string(),
            hint: Some(
                "check that the state directory exists and that its disk has room".to_string(),
            ),
        }
    }
}

/// The status, in the shape a terminal shows.
///
/// A question that is waiting goes first on its branch, because it is the only
/// line an operator can act on; everything else is context for it.
#[must_use]
pub fn status_text(status: &status::Status) -> String {
    use std::fmt::Write as _;

    let mut out = format!("store  {} events, {} bytes\n", status.events, status.bytes);
    if status.branches.is_empty() {
        out.push_str("no branches yet\n");
        return out;
    }
    for branch in &status.branches {
        let _ = write!(
            out,
            "branch {}  depth {}  messages {}",
            branch.id, branch.depth, branch.messages
        );
        if branch.archived {
            out.push_str("  archived");
        }
        if let Some(tip) = branch.tip {
            let _ = write!(out, "  tip {}", tip.rfc3339());
        }
        out.push('\n');
        if let Some(waiting) = &branch.waiting {
            let _ = writeln!(
                out,
                "  waiting: {} ({}) call {} until {}",
                waiting.tool,
                waiting.risk.as_str(),
                waiting.call,
                waiting.expires.rfc3339()
            );
            let _ = writeln!(out, "  answer with: agentctl approve {}", waiting.call);
        }
        if let Some(failure) = &branch.last_failure {
            let _ = writeln!(out, "  last failure: {failure}");
        }
    }
    out
}

/// The status, as one object.
#[must_use]
pub fn status_json(status: &status::Status) -> String {
    let branches: Vec<serde_json::Value> = status
        .branches
        .iter()
        .map(|branch| {
            json!({
                "id": branch.id.to_string(),
                "depth": branch.depth,
                "messages": branch.messages,
                "archived": branch.archived,
                "tip": branch.tip.map(|tip| tip.rfc3339()),
                "last_failure": branch.last_failure,
                "waiting": branch.waiting.as_ref().map(|waiting| json!({
                    "call": waiting.call.to_string(),
                    "expires": waiting.expires.rfc3339(),
                    "risk": waiting.risk.as_str(),
                    "tool": waiting.tool,
                })),
            })
        })
        .collect();
    json!({
        "schema": "agentctl.status/1",
        "events": status.events,
        "bytes": status.bytes,
        "branches": branches,
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::{
        fork_json, fork_text, store_failure, verify_json, verify_text, version_json, version_text,
    };
    use std::path::PathBuf;

    use agent_core::ids::{BranchId, EventId};
    use agent_store::{StoreError, VerifyReport};
    use serde_json::json;

    #[test]
    fn the_version_line_names_the_binary_and_its_version() {
        assert_eq!(
            version_text(),
            format!("agentctl {}", env!("CARGO_PKG_VERSION"))
        );
    }

    #[test]
    fn the_version_object_carries_its_schema() {
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&version_json()).expect("valid json"),
            json!({
                "schema": "agentctl.version/1",
                "version": env!("CARGO_PKG_VERSION"),
            })
        );
    }

    #[test]
    fn a_verification_prints_its_two_counts() {
        let report = VerifyReport {
            events: 12,
            branches: 3,
        };
        assert_eq!(verify_text(&report), "ok: 12 events across 3 branches");
    }

    #[test]
    fn a_verification_object_carries_its_schema_and_ok() {
        let report = VerifyReport {
            events: 12,
            branches: 3,
        };
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&verify_json(&report)).expect("valid json"),
            json!({
                "schema": "agentctl.verify/1",
                "ok": true,
                "events": 12,
                "branches": 3,
            })
        );
    }

    #[test]
    fn an_open_failure_is_a_storage_failure() {
        let failure = store_failure(&StoreError::Open {
            path: PathBuf::from("/nope/agent.db"),
            source: rusqlite::Error::InvalidQuery,
        });
        assert_eq!(failure.exit.code(), 70);
        assert_eq!(failure.class.name(), "storage");
        assert!(failure.hint.is_some_and(|hint| hint.contains("disk")));
    }

    #[test]
    fn an_identifier_failure_is_a_corrupt_store() {
        let failure = store_failure(&StoreError::Identifier("nope".to_string()));
        assert_eq!(failure.exit.code(), 74);
        assert_eq!(failure.class.name(), "corrupt");
        assert!(failure.hint.is_some_and(|hint| hint.contains("restore")));
    }

    #[test]
    fn a_chain_failure_is_a_corrupt_store() {
        let failure = store_failure(&StoreError::Chain {
            id: EventId::from_bytes([1; 32]),
            detail: "its own hash",
        });
        assert_eq!(failure.exit.code(), 74);
        assert_eq!(failure.class.name(), "corrupt");
        assert!(failure.message.contains("its own hash"));
    }

    #[test]
    fn a_position_failure_is_a_corrupt_store() {
        for error in [
            StoreError::PrevMismatch {
                branch: BranchId::from_bytes([1; 8]),
                expected: None,
                found: None,
            },
            StoreError::DepthGap {
                branch: BranchId::from_bytes([1; 8]),
                expected: 4,
                found: 6,
            },
        ] {
            assert_eq!(store_failure(&error).exit.code(), 74);
        }
    }

    #[test]
    fn an_anchor_failure_is_a_corrupt_store() {
        for error in [
            StoreError::SecondGenesis(BranchId::from_bytes([2; 8])),
            StoreError::UnknownPredecessor {
                branch: BranchId::from_bytes([2; 8]),
                prev: EventId::from_bytes([3; 32]),
            },
        ] {
            assert_eq!(store_failure(&error).exit.code(), 74);
        }
    }

    #[test]
    fn a_fork_prints_the_branch_and_its_depth() {
        assert_eq!(
            fork_text(BranchId::from_bytes([1; 8]), 3),
            "forked b-0101010101010101 at depth 3"
        );
    }

    #[test]
    fn a_fork_object_carries_the_schema_branch_and_depth_and_nothing_else() {
        assert_eq!(
            fork_json(BranchId::from_bytes([1; 8]), 3),
            r#"{"branch":"b-0101010101010101","depth":3,"schema":"agentctl.fork/1"}"#
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&fork_json(BranchId::from_bytes([1; 8]), 3))
                .expect("valid json"),
            json!({
                "schema": "agentctl.fork/1",
                "branch": "b-0101010101010101",
                "depth": 3,
            })
        );
    }

    #[test]
    fn a_query_failure_is_a_storage_failure() {
        let failure = store_failure(&StoreError::Query(rusqlite::Error::InvalidQuery));
        assert_eq!(failure.exit.code(), 70);
        assert_eq!(failure.class.name(), "storage");
        assert!(failure.hint.is_some_and(|hint| hint.contains("disk")));
    }
}
