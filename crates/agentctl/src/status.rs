//! What `agentctl status` reads out of the log.
//!
//! The status is a reading, not a request: the store is the state, so an operator
//! who needs to know what the agent is doing gets an answer whether or not the
//! daemon is running. That is the point of asking — a status that only works when
//! the daemon is up is least available exactly when it is most wanted.

use std::path::Path;

use agent_core::event::{EventKind, Risk};
use agent_core::fold::State;
use agent_core::ids::{BranchId, CallId};
use agent_core::time::Timestamp;
use agent_store::{Store, StoreError};

/// One branch, as an operator sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchStatus {
    pub id: BranchId,
    pub depth: u64,
    /// The time of the newest event on the branch.
    pub tip: Option<Timestamp>,
    pub messages: usize,
    pub archived: bool,
    /// The question waiting for this operator, if there is one.
    pub waiting: Option<Waiting>,
    /// The last thing the model said, and how it ended, when it said anything.
    pub last_failure: Option<String>,
}

/// A question that is waiting for an answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Waiting {
    pub call: CallId,
    pub expires: Timestamp,
    pub risk: Risk,
    pub tool: String,
}

/// Everything `status` reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub branches: Vec<BranchStatus>,
    pub events: u64,
    /// How large the store file is, in bytes.
    pub bytes: u64,
}

/// Reads the status out of a store.
///
/// # Errors
///
/// Returns [`StoreError`] when the store cannot be opened or read. A missing file
/// is the caller's to interpret: a daemon that has never run has no status.
pub fn read(path: &Path) -> Result<Status, StoreError> {
    let store = Store::open_read_only(path)?;
    let verified = store.verify()?;
    let branches = store.branches()?;
    let mut out = Vec::with_capacity(branches.len());
    for id in branches {
        let events = store.load(id)?;
        let mut state = State::default();
        for event in &events {
            // A fold that contradicts itself is a corrupt store, and `verify` has
            // already refused that case, so an event this build cannot fold is the
            // only thing left: it is skipped rather than failing the status.
            let _ = state.apply(event);
        }
        let Some(facts) = state.branch(id) else {
            continue;
        };
        out.push(BranchStatus {
            id,
            depth: facts.depth,
            tip: events.last().map(|event| event.time),
            messages: facts.messages.len(),
            archived: facts.archived,
            waiting: facts.approval.as_ref().map(|approval| Waiting {
                call: approval.call,
                expires: approval.expires,
                risk: approval.risk,
                tool: approval.tool.clone(),
            }),
            last_failure: last_failure(&events),
        });
    }
    let bytes = std::fs::metadata(path).map_or(0, |meta| meta.len());
    Ok(Status {
        branches: out,
        events: verified.events,
        bytes,
    })
}

/// The last failure the log records, in the words an operator needs.
fn last_failure(events: &[agent_core::event::Event]) -> Option<String> {
    events.iter().rev().find_map(|event| match &event.kind {
        EventKind::ModelFailed { class, detail, .. } => Some(detail.as_ref().map_or_else(
            || format!("model: {}", class.as_str()),
            |detail| format!("model: {} ({detail})", class.as_str()),
        )),
        EventKind::ToolFailed {
            class,
            may_have_happened,
            ..
        } => Some(format!(
            "tool: {}{}",
            class.as_str(),
            if *may_have_happened {
                ", may have happened"
            } else {
                ""
            }
        )),
        _ => None,
    })
}

#[cfg(test)]
mod tests;
