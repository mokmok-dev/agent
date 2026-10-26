//! The reactor. One loop, one writer, one decision at a time.
//!
//! The loop owns no state of its own. Every step re-reads the store and folds
//! it, so a restart, a fork and a second process all see the same facts, and a
//! decision can never run on state that an earlier decision has invalidated.

pub mod cli;
pub mod clock;

use std::fmt;
use std::path::Path;

use agent_core::decide::decide;
use agent_core::event::{Event, EventKind};
use agent_core::fold::{FoldError, State};
use agent_core::ids::{Actor, BranchId};
use agent_core::time::Timestamp;
use agent_store::{Store, StoreError};

/// The branch every store starts with. Fixed rather than random, so a fresh
/// store is reproducible and a test needs no entropy source.
pub const ROOT_BRANCH: BranchId = BranchId::from_bytes([0; 8]);

/// The actor recorded on every event the reactor writes itself.
pub const REACTOR_ACTOR: &str = "reactor";

/// Why a step could not be taken.
#[derive(Debug)]
pub enum ReactorError {
    /// The store failed to open, read, or append.
    Store(StoreError),
    /// The history contradicts itself.
    Fold(FoldError),
}

impl fmt::Display for ReactorError {
    fn fmt(
        &self,
        formatter: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        match self {
            Self::Store(error) => write!(formatter, "store: {error}"),
            Self::Fold(error) => write!(formatter, "fold: {error}"),
        }
    }
}

impl std::error::Error for ReactorError {}

impl From<StoreError> for ReactorError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

impl From<FoldError> for ReactorError {
    fn from(error: FoldError) -> Self {
        Self::Fold(error)
    }
}

/// What one step did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// An event was appended.
    Applied,
    /// There was nothing to do.
    Idle,
}

/// The process that decides and appends.
pub struct Reactor {
    store: Store,
}

impl Reactor {
    /// Opens the store behind the reactor.
    ///
    /// # Errors
    ///
    /// Returns [`ReactorError::Store`] when the database cannot be opened or
    /// verified.
    pub fn open(path: &Path) -> Result<Self, ReactorError> {
        Ok(Self {
            store: Store::open(path)?,
        })
    }

    /// Folds every branch into one state.
    ///
    /// # Errors
    ///
    /// Returns [`ReactorError::Store`] when a branch cannot be read and
    /// [`ReactorError::Fold`] when its events contradict each other.
    pub fn state(&self) -> Result<State, ReactorError> {
        let mut state = State::default();
        for branch in self.store.branches()? {
            for event in self.store.load(branch)? {
                state.apply(&event)?;
            }
        }
        Ok(state)
    }

    /// Creates the root branch and records the start, if this is a new store.
    ///
    /// # Errors
    ///
    /// Returns [`ReactorError::Store`] when either append fails.
    pub fn start(
        &mut self,
        now: Timestamp,
    ) -> Result<Step, ReactorError> {
        if !self.store.branches()?.is_empty() {
            return Ok(Step::Idle);
        }
        let created = Event {
            branch: ROOT_BRANCH,
            depth: 0,
            prev: None,
            time: now,
            actor: Actor::Process(REACTOR_ACTOR.to_string()),
            causation: None,
            correlation: None,
            model: None,
            kind: EventKind::BranchCreated {
                parent: None,
                fork: None,
                label: Some("main".to_string()),
            },
        };
        self.store.append(&created)?;

        let started = Event {
            branch: ROOT_BRANCH,
            depth: 1,
            prev: Some(created.id()),
            time: now,
            actor: Actor::Process(REACTOR_ACTOR.to_string()),
            causation: None,
            correlation: None,
            model: None,
            kind: EventKind::InstanceStarted {
                schema: agent_core::taxonomy::SCHEMA_VERSION,
                version: env!("CARGO_PKG_VERSION").to_string(),
            },
        };
        self.store.append(&started)?;
        Ok(Step::Applied)
    }

    /// Folds the history, asks for one intent, and appends it.
    ///
    /// # Errors
    ///
    /// Returns [`ReactorError::Store`] when a read or the append fails and
    /// [`ReactorError::Fold`] when the history contradicts itself.
    pub fn step(
        &mut self,
        now: Timestamp,
    ) -> Result<Step, ReactorError> {
        let state = self.state()?;
        let Some(intent) = decide(&state) else {
            return Ok(Step::Idle);
        };
        let branch = state
            .branch(intent.branch)
            .ok_or(ReactorError::Fold(FoldError::UnknownBranch(intent.branch)))?;
        let event = Event {
            branch: intent.branch,
            depth: branch.depth + 1,
            prev: branch.tip,
            time: now,
            actor: Actor::Process(REACTOR_ACTOR.to_string()),
            causation: intent.causation,
            correlation: intent.correlation,
            model: None,
            kind: intent.kind,
        };
        self.store.append(&event)?;
        Ok(Step::Applied)
    }
}

#[cfg(test)]
mod tests;
