//! The fold. State is a pure function of an ordered event slice.
//!
//! The fold keeps facts, not control flow: the transcript as it will be
//! rendered, the turns that were requested, which tool calls are still
//! unresolved, and what the cache reported. What to do next is the reactor's
//! decision and is not recorded here, so a wrong decision cannot be mistaken
//! for a wrong history.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use crate::event::{Event, EventKind, Role};
use crate::ids::{BranchId, CallId, EventId, TurnId};

/// Why an event could not be folded.
///
/// Every case is a fact that contradicts the state, never a fact this build
/// does not understand. An unknown event type folds to nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FoldError {
    /// The event names a branch that was never created.
    UnknownBranch(BranchId),
    /// A branch was created twice.
    DuplicateBranch(BranchId),
    /// The predecessor does not match the branch's tip.
    PrevMismatch {
        /// The tip the branch had.
        expected: Option<EventId>,
        /// The predecessor the event named.
        found: Option<EventId>,
    },
    /// The depth is not one past the branch's depth.
    DepthGap {
        /// The depth the branch expected.
        expected: u64,
        /// The depth the event carried.
        found: u64,
    },
    /// A turn was requested twice with the same id.
    DuplicateTurn(TurnId),
    /// The event names a turn the branch has not requested.
    UnknownTurn(TurnId),
    /// A result or failure names a call with no intent.
    UnknownCall(CallId),
}

impl core::fmt::Display for FoldError {
    fn fmt(
        &self,
        formatter: &mut core::fmt::Formatter<'_>,
    ) -> core::fmt::Result {
        match self {
            Self::UnknownBranch(id) => write!(formatter, "unknown branch {id}"),
            Self::DuplicateBranch(id) => write!(formatter, "branch {id} already exists"),
            Self::PrevMismatch { expected, found } => {
                write!(
                    formatter,
                    "predecessor mismatch: expected {expected:?}, found {found:?}"
                )
            },
            Self::DepthGap { expected, found } => {
                write!(formatter, "depth gap: expected {expected}, found {found}")
            },
            Self::DuplicateTurn(id) => write!(formatter, "turn {id} already requested"),
            Self::UnknownTurn(id) => write!(formatter, "unknown turn {id}"),
            Self::UnknownCall(id) => write!(formatter, "unknown call {id}"),
        }
    }
}

impl core::error::Error for FoldError {}

/// One message, as the prompt will see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub role: Role,
    pub content: String,
    pub tokens: u64,
    pub turn: Option<TurnId>,
}

/// A command this branch has received and not yet turned into work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    pub event: EventId,
    pub name: String,
    pub text: Option<String>,
}

/// A turn that was requested on this branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Turn {
    pub id: TurnId,
    pub input: String,
}

/// What the provider reported about prefix reuse on this branch.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Cache {
    pub cached: u64,
    pub uncached: u64,
    pub reported: u64,
    pub unreported: u64,
}

/// One branch of history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Branch {
    pub parent: Option<BranchId>,
    pub fork: Option<EventId>,
    pub label: Option<String>,
    pub archived: bool,
    pub depth: u64,
    pub tip: Option<EventId>,
    pub messages: Vec<Message>,
    pub turns: Vec<Turn>,
    pub pending: BTreeMap<CallId, String>,
    pub pending_command: Option<Command>,
    pub cache: Cache,
    pub failures: u32,
}

impl Branch {
    fn new(
        event: &Event,
        parent: Option<BranchId>,
        fork: Option<EventId>,
        label: Option<String>,
    ) -> Self {
        Self {
            parent,
            fork,
            label,
            archived: false,
            depth: event.depth,
            tip: Some(event.id()),
            messages: Vec::new(),
            turns: Vec::new(),
            pending: BTreeMap::new(),
            pending_command: None,
            cache: Cache::default(),
            failures: 0,
        }
    }

    fn has_turn(
        &self,
        turn: TurnId,
    ) -> bool {
        self.turns.iter().any(|known| known.id == turn)
    }
}

/// Everything the fold knows, one entry per branch.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct State {
    branches: BTreeMap<BranchId, Branch>,
}

impl State {
    #[must_use]
    pub fn branch(
        &self,
        id: BranchId,
    ) -> Option<&Branch> {
        self.branches.get(&id)
    }

    pub fn branches(&self) -> impl Iterator<Item = (&BranchId, &Branch)> {
        self.branches.iter()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.branches.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.branches.is_empty()
    }

    /// Applies one event.
    ///
    /// # Errors
    ///
    /// Returns a [`FoldError`] when the event contradicts the state: an unknown
    /// branch, a predecessor that does not match the tip, a depth that skips, a
    /// turn or call that was never opened.
    #[allow(
        clippy::too_many_lines,
        reason = "one flat match over the taxonomy is the table itself; splitting it by event family would put the invariants of one branch in two places"
    )]
    pub fn apply(
        &mut self,
        event: &Event,
    ) -> Result<(), FoldError> {
        if let EventKind::BranchCreated {
            parent,
            fork,
            label,
        } = &event.kind
        {
            if self.branches.contains_key(&event.branch) {
                return Err(FoldError::DuplicateBranch(event.branch));
            }
            self.branches.insert(
                event.branch,
                Branch::new(event, *parent, *fork, label.clone()),
            );
            return Ok(());
        }

        let branch = self
            .branches
            .get_mut(&event.branch)
            .ok_or(FoldError::UnknownBranch(event.branch))?;

        if event.prev != branch.tip {
            return Err(FoldError::PrevMismatch {
                expected: branch.tip,
                found: event.prev,
            });
        }
        let expected = branch.depth + 1;
        if event.depth != expected {
            return Err(FoldError::DepthGap {
                expected,
                found: event.depth,
            });
        }
        branch.depth = event.depth;
        branch.tip = Some(event.id());

        match &event.kind {
            EventKind::BranchLabeled { label } => branch.label = Some(label.clone()),
            EventKind::BranchArchived { .. } => branch.archived = true,
            EventKind::TurnRequested { input, turn } => {
                if branch.has_turn(*turn) {
                    return Err(FoldError::DuplicateTurn(*turn));
                }
                branch.turns.push(Turn {
                    id: *turn,
                    input: input.clone(),
                });
                branch.pending_command = None;
            },
            EventKind::CommandReceived { command, text, .. } => {
                branch.pending_command = Some(Command {
                    event: event.id(),
                    name: command.clone(),
                    text: text.clone(),
                });
            },
            EventKind::ModelRequested { turn, .. }
            | EventKind::ModelResponded { turn, .. }
            | EventKind::ModelFailed { turn, .. } => {
                if !branch.has_turn(*turn) {
                    return Err(FoldError::UnknownTurn(*turn));
                }
                if matches!(event.kind, EventKind::ModelFailed { .. }) {
                    branch.failures += 1;
                }
            },
            EventKind::ModelCacheObserved {
                cached,
                reported,
                uncached,
                ..
            } => {
                branch.cache.cached += cached;
                branch.cache.uncached += uncached;
                if *reported {
                    branch.cache.reported += 1;
                } else {
                    branch.cache.unreported += 1;
                }
            },
            EventKind::MessageAppended {
                content,
                role,
                tokens,
            } => branch.messages.push(Message {
                role: *role,
                content: content.clone(),
                tokens: *tokens,
                turn: event.correlation,
            }),
            EventKind::ToolIntent { call, tool, .. } => {
                branch.pending.insert(*call, tool.clone());
            },
            EventKind::ToolResult { call, .. } | EventKind::ToolFailed { call, .. } => {
                if branch.pending.remove(call).is_none() {
                    return Err(FoldError::UnknownCall(*call));
                }
            },
            EventKind::BranchCreated { .. }
            | EventKind::InstanceStarted { .. }
            | EventKind::InstanceStopping { .. }
            | EventKind::InstanceRecovered { .. }
            | EventKind::InstanceClockSkew { .. }
            | EventKind::Unknown { .. } => {},
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{FoldError, State};
    use crate::event::{ErrorClass, Event, EventKind, FinishReason, Role};
    use crate::ids::{Actor, BranchId, CallId, EventId, PrefixHash, TurnId};
    use crate::time::Timestamp;
    use alloc::collections::BTreeMap;
    use alloc::string::{String, ToString};

    const BRANCH: [u8; 8] = [1; 8];
    const OTHER: [u8; 8] = [2; 8];
    const TURN: [u8; 8] = [3; 8];

    struct Chain {
        branch: BranchId,
        depth: u64,
        prev: Option<EventId>,
        time: Timestamp,
    }

    impl Chain {
        fn new(branch: BranchId) -> Self {
            Self {
                branch,
                depth: 0,
                prev: None,
                time: Timestamp::new(1_790_000_000, 0).expect("valid"),
            }
        }

        fn event(
            &self,
            kind: EventKind,
        ) -> Event {
            Event {
                branch: self.branch,
                depth: self.depth,
                prev: self.prev,
                time: self.time,
                actor: Actor::Process("test".to_string()),
                causation: None,
                correlation: None,
                model: None,
                kind,
            }
        }

        fn push(
            &mut self,
            kind: EventKind,
        ) -> Event {
            let event = self.event(kind);
            self.prev = Some(event.id());
            self.depth += 1;
            event
        }
    }

    fn turn() -> TurnId {
        TurnId::from_bytes(TURN)
    }

    fn call() -> CallId {
        CallId {
            turn: turn(),
            step: 0,
        }
    }

    fn created(chain: &mut Chain) -> Event {
        chain.push(EventKind::BranchCreated {
            parent: None,
            fork: None,
            label: None,
        })
    }

    fn with_branch() -> (Chain, State) {
        let mut chain = Chain::new(BranchId::from_bytes(BRANCH));
        let mut state = State::default();
        state.apply(&created(&mut chain)).expect("creates");
        (chain, state)
    }

    #[test]
    fn a_created_branch_starts_at_its_depth() {
        let mut chain = Chain::new(BranchId::from_bytes(BRANCH));
        chain.depth = 4;
        let mut state = State::default();
        state
            .apply(&chain.push(EventKind::BranchCreated {
                parent: Some(BranchId::from_bytes(OTHER)),
                fork: Some(EventId::from_bytes([9; 32])),
                label: Some("retry".to_string()),
            }))
            .expect("creates");
        let branch = state.branch(BranchId::from_bytes(BRANCH)).expect("exists");
        assert_eq!(branch.depth, 4);
        assert_eq!(branch.parent, Some(BranchId::from_bytes(OTHER)));
        assert_eq!(branch.fork, Some(EventId::from_bytes([9; 32])));
        assert_eq!(branch.label.as_deref(), Some("retry"));
        assert!(!branch.archived);
        assert_eq!(state.len(), 1);
    }

    #[test]
    fn a_second_creation_of_the_same_branch_is_an_error() {
        let (mut chain, mut state) = with_branch();
        let again = chain.push(EventKind::BranchCreated {
            parent: None,
            fork: None,
            label: None,
        });
        assert_eq!(
            state.apply(&again),
            Err(FoldError::DuplicateBranch(BranchId::from_bytes(BRANCH)))
        );
    }

    #[test]
    fn an_event_before_creation_is_an_error() {
        let chain = Chain::new(BranchId::from_bytes(BRANCH));
        let mut state = State::default();
        assert_eq!(
            state.apply(&chain.event(EventKind::InstanceStopping {
                reason: "test".to_string()
            })),
            Err(FoldError::UnknownBranch(BranchId::from_bytes(BRANCH)))
        );
        assert!(state.is_empty());
    }

    #[test]
    fn a_predecessor_that_is_not_the_tip_is_an_error() {
        let (chain, mut state) = with_branch();
        let mut event = chain.event(EventKind::InstanceRecovered { rolled_back: false });
        event.prev = Some(EventId::from_bytes([7; 32]));
        assert_eq!(
            state.apply(&event),
            Err(FoldError::PrevMismatch {
                expected: Some(EventId::from_bytes([7; 32])).map(|_| chain.prev.expect("tip")),
                found: Some(EventId::from_bytes([7; 32])),
            })
        );
    }

    #[test]
    fn a_depth_that_skips_is_an_error() {
        let (chain, mut state) = with_branch();
        let mut event = chain.event(EventKind::InstanceRecovered { rolled_back: false });
        event.depth = 9;
        assert_eq!(
            state.apply(&event),
            Err(FoldError::DepthGap {
                expected: 1,
                found: 9
            })
        );
    }

    #[test]
    fn the_tip_advances_with_each_event() {
        let (mut chain, mut state) = with_branch();
        let first = chain.push(EventKind::InstanceRecovered { rolled_back: true });
        state.apply(&first).expect("folds");
        let branch = state.branch(BranchId::from_bytes(BRANCH)).expect("exists");
        assert_eq!(branch.tip, Some(first.id()));
        assert_eq!(branch.depth, 1);
    }

    #[test]
    fn a_label_is_recorded_and_survives() {
        let (mut chain, mut state) = with_branch();
        let labeled = chain.push(EventKind::BranchLabeled {
            label: "main".to_string(),
        });
        state.apply(&labeled).expect("folds");
        assert_eq!(
            state
                .branch(BranchId::from_bytes(BRANCH))
                .expect("exists")
                .label
                .as_deref(),
            Some("main")
        );
    }

    #[test]
    fn an_archived_branch_is_marked() {
        let (mut chain, mut state) = with_branch();
        let archived = chain.push(EventKind::BranchArchived {
            reason: Some("done".to_string()),
        });
        state.apply(&archived).expect("folds");
        assert!(
            state
                .branch(BranchId::from_bytes(BRANCH))
                .expect("exists")
                .archived
        );
    }

    #[test]
    fn a_turn_is_recorded_with_its_input() {
        let (mut chain, mut state) = with_branch();
        let requested = chain.push(EventKind::TurnRequested {
            input: "hello".to_string(),
            turn: turn(),
        });
        state.apply(&requested).expect("folds");
        let branch = state.branch(BranchId::from_bytes(BRANCH)).expect("exists");
        assert_eq!(branch.turns.len(), 1);
        assert_eq!(branch.turns[0].id, turn());
        assert_eq!(branch.turns[0].input, "hello");
    }

    #[test]
    fn the_same_turn_twice_is_an_error() {
        let (mut chain, mut state) = with_branch();
        let first = chain.push(EventKind::TurnRequested {
            input: "hello".to_string(),
            turn: turn(),
        });
        state.apply(&first).expect("folds");
        let second = chain.push(EventKind::TurnRequested {
            input: "again".to_string(),
            turn: turn(),
        });
        assert_eq!(state.apply(&second), Err(FoldError::DuplicateTurn(turn())));
    }

    #[test]
    fn a_model_event_for_an_unknown_turn_is_an_error() {
        let (mut chain, mut state) = with_branch();
        let unknown = chain.push(EventKind::ModelRequested {
            prefix: PrefixHash::from_bytes([0; 32]),
            segments: 1,
            tokens: 1,
            turn: turn(),
            warm: false,
        });
        assert_eq!(state.apply(&unknown), Err(FoldError::UnknownTurn(turn())));
    }

    #[test]
    fn a_model_event_for_a_known_turn_folds() {
        let (mut chain, mut state) = with_branch();
        let requested = chain.push(EventKind::TurnRequested {
            input: "hello".to_string(),
            turn: turn(),
        });
        state.apply(&requested).expect("folds");
        let model = chain.push(EventKind::ModelRequested {
            prefix: PrefixHash::from_bytes([0; 32]),
            segments: 1,
            tokens: 1,
            turn: turn(),
            warm: false,
        });
        state.apply(&model).expect("folds");
        let responded = chain.push(EventKind::ModelResponded {
            finish: FinishReason::Stop,
            input_tokens: 1,
            output: "hi".to_string(),
            output_tokens: 1,
            request: "r".to_string(),
            turn: turn(),
        });
        state.apply(&responded).expect("folds");
        assert_eq!(
            state
                .branch(BranchId::from_bytes(BRANCH))
                .expect("exists")
                .failures,
            0
        );
    }

    #[test]
    fn a_failure_is_counted() {
        let (mut chain, mut state) = with_branch();
        let requested = chain.push(EventKind::TurnRequested {
            input: "hello".to_string(),
            turn: turn(),
        });
        state.apply(&requested).expect("folds");
        let failed = chain.push(EventKind::ModelFailed {
            attempt: 1,
            class: ErrorClass::Timeout,
            retryable: true,
            turn: turn(),
        });
        state.apply(&failed).expect("folds");
        let branch = state.branch(BranchId::from_bytes(BRANCH)).expect("exists");
        assert_eq!(branch.failures, 1);
    }

    #[test]
    fn a_message_keeps_its_role_content_and_turn() {
        let (chain, mut state) = with_branch();
        let mut appended = chain.event(EventKind::MessageAppended {
            content: "hi".to_string(),
            role: Role::Assistant,
            tokens: 3,
        });
        appended.correlation = Some(turn());
        state.apply(&appended).expect("folds");
        let branch = state.branch(BranchId::from_bytes(BRANCH)).expect("exists");
        assert_eq!(branch.messages.len(), 1);
        assert_eq!(branch.messages[0].role, Role::Assistant);
        assert_eq!(branch.messages[0].content, "hi");
        assert_eq!(branch.messages[0].tokens, 3);
        assert_eq!(branch.messages[0].turn, Some(turn()));
    }

    #[test]
    fn cache_observations_accumulate_and_count_reported() {
        let (mut chain, mut state) = with_branch();
        let reported = chain.push(EventKind::ModelCacheObserved {
            cached: 8,
            prefix: PrefixHash::from_bytes([0; 32]),
            reported: true,
            turn: turn(),
            uncached: 2,
        });
        state.apply(&reported).expect("folds");
        let unreported = chain.push(EventKind::ModelCacheObserved {
            cached: 0,
            prefix: PrefixHash::from_bytes([0; 32]),
            reported: false,
            turn: turn(),
            uncached: 5,
        });
        state.apply(&unreported).expect("folds");
        let cache = state
            .branch(BranchId::from_bytes(BRANCH))
            .expect("exists")
            .cache;
        assert_eq!(cache.cached, 8);
        assert_eq!(cache.uncached, 7);
        assert_eq!(cache.reported, 1);
        assert_eq!(cache.unreported, 1);
    }

    #[test]
    fn an_intent_is_pending_until_its_result() {
        let (mut chain, mut state) = with_branch();
        let intent = chain.push(EventKind::ToolIntent {
            args: "{}".to_string(),
            call: call(),
            key: "k".to_string(),
            tool: "shell".to_string(),
        });
        state.apply(&intent).expect("folds");
        assert_eq!(
            state
                .branch(BranchId::from_bytes(BRANCH))
                .expect("exists")
                .pending
                .get(&call()),
            Some(&"shell".to_string())
        );
        let result = chain.push(EventKind::ToolResult {
            call: call(),
            duration_ms: 1,
            status: 0,
            truncated: false,
        });
        state.apply(&result).expect("folds");
        assert!(
            state
                .branch(BranchId::from_bytes(BRANCH))
                .expect("exists")
                .pending
                .is_empty()
        );
    }

    #[test]
    fn a_failure_also_clears_a_pending_intent() {
        let (mut chain, mut state) = with_branch();
        let intent = chain.push(EventKind::ToolIntent {
            args: "{}".to_string(),
            call: call(),
            key: "k".to_string(),
            tool: "shell".to_string(),
        });
        state.apply(&intent).expect("folds");
        let failed = chain.push(EventKind::ToolFailed {
            call: call(),
            class: ErrorClass::Unknown,
            may_have_happened: true,
        });
        state.apply(&failed).expect("folds");
        assert!(
            state
                .branch(BranchId::from_bytes(BRANCH))
                .expect("exists")
                .pending
                .is_empty()
        );
    }

    #[test]
    fn a_result_without_an_intent_is_an_error() {
        let (mut chain, mut state) = with_branch();
        let orphan = chain.push(EventKind::ToolResult {
            call: call(),
            duration_ms: 1,
            status: 0,
            truncated: false,
        });
        assert_eq!(state.apply(&orphan), Err(FoldError::UnknownCall(call())));
    }

    #[test]
    fn instance_and_command_events_advance_the_chain_without_changing_facts() {
        let (mut chain, mut state) = with_branch();
        for kind in [
            EventKind::InstanceStarted {
                schema: 1,
                version: "0.0.0".to_string(),
            },
            EventKind::CommandReceived {
                command: "send".to_string(),
                command_id: "c".to_string(),
                surface: "tui".to_string(),
                text: None,
            },
            EventKind::Unknown {
                schema: 1,
                data: "{}".to_string(),
            },
        ] {
            let event = chain.push(kind);
            state.apply(&event).expect("folds");
        }
        let branch = state.branch(BranchId::from_bytes(BRANCH)).expect("exists");
        assert_eq!(branch.depth, 3);
        assert_eq!(branch.messages.len(), 0);
        assert_eq!(branch.turns.len(), 0);
    }

    #[test]
    fn two_branches_fold_independently() {
        let mut state = State::default();
        let mut left = Chain::new(BranchId::from_bytes(BRANCH));
        let mut right = Chain::new(BranchId::from_bytes(OTHER));
        state.apply(&created(&mut left)).expect("creates");
        state.apply(&created(&mut right)).expect("creates");
        let event = left.push(EventKind::BranchLabeled {
            label: "left".to_string(),
        });
        state.apply(&event).expect("folds");
        assert_eq!(state.len(), 2);
        assert!(!state.is_empty());
        assert_eq!(
            state
                .branch(BranchId::from_bytes(BRANCH))
                .expect("exists")
                .label
                .as_deref(),
            Some("left")
        );
        assert!(
            state
                .branch(BranchId::from_bytes(OTHER))
                .expect("exists")
                .label
                .is_none()
        );
    }

    #[test]
    fn folding_the_same_slice_twice_gives_the_same_state() {
        let mut chain = Chain::new(BranchId::from_bytes(BRANCH));
        let events = alloc::vec![
            created(&mut chain),
            chain.push(EventKind::TurnRequested {
                input: "hello".to_string(),
                turn: turn(),
            }),
            chain.push(EventKind::MessageAppended {
                content: "hi".to_string(),
                role: Role::Assistant,
                tokens: 1,
            }),
        ];
        let mut first = State::default();
        let mut second = State::default();
        for event in &events {
            first.apply(event).expect("folds");
            second.apply(event).expect("folds");
        }
        assert_eq!(first, second);
    }

    #[test]
    fn the_branch_map_is_readable() {
        let (_chain, state) = with_branch();
        let ids: BTreeMap<BranchId, usize> = state.branches().map(|(id, _)| (*id, 1)).collect();
        assert_eq!(ids.len(), 1);
        assert!(ids.contains_key(&BranchId::from_bytes(BRANCH)));
    }

    #[test]
    fn every_error_explains_itself() {
        use alloc::format;
        let errors = [
            FoldError::UnknownBranch(BranchId::from_bytes(BRANCH)),
            FoldError::DuplicateBranch(BranchId::from_bytes(BRANCH)),
            FoldError::PrevMismatch {
                expected: None,
                found: None,
            },
            FoldError::DepthGap {
                expected: 1,
                found: 2,
            },
            FoldError::DuplicateTurn(turn()),
            FoldError::UnknownTurn(turn()),
            FoldError::UnknownCall(call()),
        ];
        for error in errors {
            assert_ne!(format!("{error}"), "");
        }
    }

    #[test]
    fn the_state_renders_nothing_of_its_own() {
        let empty = State::default();
        assert!(empty.is_empty());
        assert_eq!(empty.len(), 0);
    }

    #[test]
    fn an_unrelated_branch_id_is_none() {
        let (_chain, state) = with_branch();
        assert!(state.branch(BranchId::from_bytes(OTHER)).is_none());
    }

    #[test]
    fn a_message_carries_no_turn_when_the_event_had_none() {
        let (mut chain, mut state) = with_branch();
        let event = chain.push(EventKind::MessageAppended {
            content: String::from("hi"),
            role: Role::User,
            tokens: 1,
        });
        state.apply(&event).expect("folds");
        assert_eq!(
            state
                .branch(BranchId::from_bytes(BRANCH))
                .expect("exists")
                .messages[0]
                .turn,
            None
        );
    }
}
