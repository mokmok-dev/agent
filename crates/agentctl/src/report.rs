//! What a command prints, and how a store failure becomes the shared error.
//!
//! Every shape here is one the interface spec fixes: text for a person, one
//! object with a `schema` field for a script. Nothing in this module prints, so
//! its output is asserted as a string rather than captured from a terminal.

use agent_store::{StoreError, VerifyReport};
use serde_json::json;

use crate::exit::{Class, Exit, Failure};

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

/// The failure a store error becomes.
///
/// Storage that could not be read is a storage failure an operator fixes by
/// making room or fixing a path. Data that no longer matches what the writer
/// promised is a corrupt store, and the remedy is a restore, not a repair.
#[must_use]
pub fn store_failure(error: &StoreError) -> Failure {
    let corrupt = matches!(
        error,
        StoreError::Identifier(_)
            | StoreError::PrevMismatch { .. }
            | StoreError::DepthGap { .. }
            | StoreError::Chain { .. }
            | StoreError::Decode(_)
    );
    if corrupt {
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

#[cfg(test)]
mod tests {
    use super::{store_failure, verify_json, verify_text, version_json, version_text};
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
    fn a_query_failure_is_a_storage_failure() {
        let failure = store_failure(&StoreError::Query(rusqlite::Error::InvalidQuery));
        assert_eq!(failure.exit.code(), 70);
        assert_eq!(failure.class.name(), "storage");
        assert!(failure.hint.is_some_and(|hint| hint.contains("disk")));
    }
}
