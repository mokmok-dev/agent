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
pub use agent_core::decide::{Intent, Step};

/// How long to sleep before an instant, capped so a clock jump cannot park the loop.
#[must_use]
pub fn wait_for(
    now: Timestamp,
    until: Timestamp,
) -> core::time::Duration {
    let seconds = until.secs().saturating_sub(now.secs());
    let millis = u64::try_from(seconds).unwrap_or(0).saturating_mul(1000);
    core::time::Duration::from_millis(millis).min(MAX_WAIT)
}

/// The longest the loop will sleep for a single retry.
pub const MAX_WAIT: core::time::Duration = core::time::Duration::from_secs(5);
use agent_core::event::{Event, EventKind, FinishReason, Role};
use agent_core::fold::{FoldError, State};
use agent_core::ids::{Actor, BranchId, EventId, TurnId};
use agent_core::time::{TimeError, Timestamp};
use agent_model::{Provider, Purpose, Request, RequestRecord, backoff};
use agent_prompt::{Directive, ToolSchema, render};
use agent_store::{Store, StoreError};

/// The branch every store starts with. Fixed rather than random, so a fresh
/// store is reproducible and a test needs no entropy source.
pub const ROOT_BRANCH: BranchId = BranchId::from_bytes([0; 8]);

/// The actor recorded on every event the reactor writes itself.
pub const REACTOR_ACTOR: &str = "reactor";

/// Whether a startup step did anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Work {
    Done,
    Nothing,
}

/// What the reactor needs in order to run a turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub model: String,
    pub directive_version: String,
    pub directive: String,
    /// Tool names and their schemas, in any order; the renderer sorts them.
    pub tools: Vec<(String, String)>,
    pub max_output_tokens: u64,
}

/// A jitter draw taken from the request's own id.
///
/// The delay then depends only on the log, so a test can predict it and a replay
/// reproduces it. It is not random across runs, which costs a little of the
/// herd-spreading that randomness would buy; a single agent talking to one
/// provider does not have a herd.
fn draw_of(request: EventId) -> u64 {
    let bytes = request.as_bytes();
    let mut draw = [0_u8; 8];
    draw.copy_from_slice(&bytes[..8]);
    u64::from_le_bytes(draw)
}

/// Why a step could not be taken.
#[derive(Debug)]
pub enum ReactorError {
    /// The store failed to open, read, or append.
    Store(StoreError),
    /// The history contradicts itself.
    Fold(FoldError),
    /// A turn is ready but this reactor has no model to send it to.
    NoModel,
    /// An instant could not be computed.
    Time(TimeError),
}

impl fmt::Display for ReactorError {
    fn fmt(
        &self,
        formatter: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        match self {
            Self::Store(error) => write!(formatter, "store: {error}"),
            Self::Fold(error) => write!(formatter, "fold: {error}"),
            Self::NoModel => write!(formatter, "a turn is ready but no model is configured"),
            Self::Time(error) => write!(formatter, "time: {error}"),
        }
    }
}

impl std::error::Error for ReactorError {}

impl From<StoreError> for ReactorError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

impl From<TimeError> for ReactorError {
    fn from(error: TimeError) -> Self {
        Self::Time(error)
    }
}

impl From<FoldError> for ReactorError {
    fn from(error: FoldError) -> Self {
        Self::Fold(error)
    }
}

/// The process that decides and appends.
pub struct Reactor {
    store: Store,
    provider: Option<Box<dyn Provider>>,
    settings: Option<Settings>,
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
            provider: None,
            settings: None,
        })
    }

    /// Opens the store and gives the reactor a model to send turns to.
    ///
    /// # Errors
    ///
    /// Returns [`ReactorError::Store`] when the database cannot be opened or
    /// verified.
    pub fn with_model(
        path: &Path,
        provider: Box<dyn Provider>,
        settings: Settings,
    ) -> Result<Self, ReactorError> {
        let mut reactor = Self::open(path)?;
        reactor.provider = Some(provider);
        reactor.settings = Some(settings);
        Ok(reactor)
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
    ) -> Result<Work, ReactorError> {
        if !self.store.branches()?.is_empty() {
            return Ok(Work::Nothing);
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
        Ok(Work::Done)
    }

    /// Folds the history, asks for one step, and carries it out.
    ///
    /// # Errors
    ///
    /// Returns [`ReactorError::Store`] when a read or an append fails,
    /// [`ReactorError::Fold`] when the history contradicts itself, and
    /// [`ReactorError::NoModel`] when a turn is ready but no model is configured.
    pub fn step(
        &mut self,
        now: Timestamp,
    ) -> Result<Step, ReactorError> {
        let state = self.state()?;
        let step = decide(&state, now);
        match &step {
            Step::Append(intent) => {
                self.append(
                    intent.branch,
                    &state,
                    now,
                    None,
                    intent.causation,
                    intent.correlation,
                    intent.kind.clone(),
                )?;
            },
            Step::RequestModel { branch, turn } => {
                self.run_turn(&state, *branch, *turn, now)?;
            },
            Step::Wait { .. } | Step::Idle => {},
        }
        Ok(step)
    }

    /// Renders one turn, records the request, and records what came back.
    fn run_turn(
        &mut self,
        state: &State,
        branch: BranchId,
        turn: TurnId,
        now: Timestamp,
    ) -> Result<(), ReactorError> {
        let Some(mut provider) = self.provider.take() else {
            return Err(ReactorError::NoModel);
        };
        let Some(settings) = self.settings.clone() else {
            self.provider = Some(provider);
            return Err(ReactorError::NoModel);
        };
        let facts = state
            .branch(branch)
            .ok_or(ReactorError::Fold(FoldError::UnknownBranch(branch)))?;
        let mut messages: Vec<(Role, &str)> = facts
            .messages
            .iter()
            .map(|message| (message.role, message.content.as_str()))
            .collect();
        let asked = facts
            .turns
            .iter()
            .find(|candidate| candidate.id == turn)
            .ok_or(ReactorError::Fold(FoldError::UnknownTurn(turn)))?;
        messages.push((Role::User, asked.input.as_str()));
        let tools: Vec<ToolSchema<'_>> = settings
            .tools
            .iter()
            .map(|(name, schema)| ToolSchema {
                name: name.as_str(),
                schema: schema.as_str(),
            })
            .collect();
        let directive = Directive {
            version: &settings.directive_version,
            text: &settings.directive,
        };
        let prompt = render(&directive, &tools, &messages);
        let Some(record) = RequestRecord::of(&prompt, Purpose::Turn) else {
            self.provider = Some(provider);
            return Err(ReactorError::NoModel);
        };
        let requested = self.append(
            branch,
            state,
            now,
            None,
            None,
            Some(turn),
            EventKind::ModelRequested {
                prefix: record.prefix,
                segments: record.segments,
                tokens: record.tokens,
                turn,
                warm: false,
            },
        )?;

        let request = Request::of(&prompt, &settings.model, settings.max_output_tokens);
        let mut output = String::new();
        let result = provider.call(&request, &mut |delta| match delta {
            agent_model::Delta::Text(piece) => output.push_str(piece),
        });
        self.provider = Some(provider);

        match result {
            Ok(completion) => {
                let state_after = self.state()?;
                self.append(
                    branch,
                    &state_after,
                    now,
                    Some(settings.model.clone()),
                    Some(requested),
                    Some(turn),
                    EventKind::ModelResponded {
                        finish: completion.finish.unwrap_or(FinishReason::Unknown),
                        input_tokens: completion.usage.input_tokens,
                        output,
                        output_tokens: completion.usage.output_tokens,
                        request: requested.to_string(),
                        turn,
                    },
                )?;
            },
            Err(error) => self.record_failure(branch, turn, requested, &error, now)?,
        }
        Ok(())
    }

    /// Writes the failure, and the instant the next attempt is owed, if it is.
    fn record_failure(
        &mut self,
        branch: BranchId,
        turn: TurnId,
        requested: EventId,
        error: &agent_model::ProviderError,
        now: Timestamp,
    ) -> Result<(), ReactorError> {
        let state = self.state()?;
        let attempt = state
            .branch(branch)
            .and_then(|facts| facts.pending_model.as_ref())
            .map_or(1, |pending| pending.attempts.max(1));
        let retryable = error.retryable(false);
        self.append(
            branch,
            &state,
            now,
            Some(
                self.settings
                    .as_ref()
                    .map(|settings| settings.model.clone())
                    .unwrap_or_default(),
            ),
            Some(requested),
            Some(turn),
            EventKind::ModelFailed {
                attempt,
                class: error.class(),
                retryable,
                turn,
            },
        )?;
        if !retryable {
            return Ok(());
        }
        let Some(delay) = backoff(attempt, draw_of(requested)) else {
            return Ok(());
        };
        let due = now.plus(delay)?;
        let armed_state = self.state()?;
        self.append(
            branch,
            &armed_state,
            now,
            None,
            Some(requested),
            Some(turn),
            EventKind::ScheduleArmed { attempt, due, turn },
        )?;
        Ok(())
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "every argument is a fact the event needs; a builder would hide which of them the caller chose"
    )]
    fn append(
        &mut self,
        branch: BranchId,
        state: &State,
        now: Timestamp,
        model: Option<String>,
        causation: Option<EventId>,
        correlation: Option<TurnId>,
        kind: EventKind,
    ) -> Result<EventId, ReactorError> {
        let facts = state
            .branch(branch)
            .ok_or(ReactorError::Fold(FoldError::UnknownBranch(branch)))?;
        let event = Event {
            branch,
            depth: facts.depth + 1,
            prev: facts.tip,
            time: now,
            actor: Actor::Process(REACTOR_ACTOR.to_string()),
            causation,
            correlation,
            model,
            kind,
        };
        self.store.append(&event)?;
        Ok(event.id())
    }
}

#[cfg(test)]
mod tests;
