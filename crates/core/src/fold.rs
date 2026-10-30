//! The fold. State is a pure function of an ordered event slice.
//!
//! The fold keeps facts, not control flow: the transcript as it will be
//! rendered, the turns that were requested, which tool calls are still
//! unresolved, and what the cache reported. What to do next is the reactor's
//! decision and is not recorded here, so a wrong decision cannot be mistaken
//! for a wrong history.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::String;
use alloc::vec::Vec;

use crate::event::{ErrorClass, Event, EventKind, Risk, Role, ToolRequest};
use crate::ids::{BranchId, CallId, EventId, TurnId};
use crate::time::Timestamp;

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
    /// The same step asked for a tool twice, which only a second writer can do.
    RepeatedCall(CallId),
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
            Self::RepeatedCall(id) => write!(formatter, "call {id} was filed twice"),
        }
    }
}

impl core::error::Error for FoldError {}

/// A question waiting for a person, and the instant it stops being askable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Approval {
    pub call: CallId,
    pub expires: Timestamp,
    pub risk: Risk,
    pub tool: String,
}

/// How many times one turn may ask for tools before it stops asking.
pub const MAX_TOOL_ROUNDS: usize = 16;

/// One call a message asked for, with the id the answer will carry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskedCall {
    pub call: CallId,
    pub args: String,
    pub tool: String,
}

/// One message, as the prompt will see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub role: Role,
    /// The calls this message asked for, when it is the answer that asked.
    pub calls: Vec<AskedCall>,
    /// The call this message answers, when it is a tool's output.
    pub call: Option<CallId>,
    /// The depth of the event that projected this message.
    pub depth: u64,
    pub content: String,
    /// The token count the writer reported, when there was one to report.
    ///
    /// An entry the fold derives from `turn.requested` has none: nobody counted
    /// it, and a zero would claim somebody did.
    pub tokens: Option<u64>,
    pub turn: Option<TurnId>,
}

/// A command this branch has received and not yet turned into work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    pub event: EventId,
    pub name: String,
    pub text: Option<String>,
}

/// A turn that has been asked for and has not finished.
///
/// The attempts count is what the retry policy reads, and the due instant is
/// what a restart resumes from, so neither has to be remembered anywhere else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingModel {
    pub turn: TurnId,
    pub attempts: u32,
    pub due: Option<Timestamp>,
}

/// An answer the model gave that the transcript does not hold yet.
///
/// The reply is recorded by `model.responded` and by nothing else, so a process
/// that stops between that event and the transcript entry would otherwise leave
/// an answer no later turn can see. Keeping it here makes the entry a decision
/// the loop can repeat after a restart.
/// One response's calls, with the step its first call carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskedRound {
    pub base: u32,
    pub requests: Vec<ToolRequest>,
    pub filed: u32,
}

/// What one turn asked the tools for, one round per response.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Asked {
    pub rounds: Vec<AskedRound>,
}

impl Asked {
    /// Adds a response's calls. Old events carry no base, and zero is the id
    /// scheme they were written with.
    pub fn push(
        &mut self,
        base: u32,
        requests: Vec<ToolRequest>,
    ) {
        self.rounds.push(AskedRound {
            base,
            requests,
            filed: 0,
        });
    }

    /// The first call whose intent is not written yet, with the id it will carry.
    #[must_use]
    pub fn next_unfiled(
        &self,
        turn: TurnId,
    ) -> Option<(CallId, &ToolRequest)> {
        for round in &self.rounds {
            if let Some(request) = round.requests.get(round.filed as usize) {
                return Some((
                    CallId {
                        turn,
                        step: round.base.saturating_add(round.filed),
                    },
                    request,
                ));
            }
        }
        None
    }

    /// The request a call names. The latest round wins where old rounds overlap.
    #[must_use]
    pub fn request(
        &self,
        call: CallId,
    ) -> Option<&ToolRequest> {
        for round in self.rounds.iter().rev() {
            let length = u32::try_from(round.requests.len()).ok()?;
            let end = round.base.saturating_add(length);
            if call.step >= round.base && call.step < end {
                return round.requests.get((call.step - round.base) as usize);
            }
        }
        None
    }

    /// Records that a call's intent is written.
    pub fn mark_filed(
        &mut self,
        call: CallId,
    ) {
        for round in self.rounds.iter_mut().rev() {
            let length = u32::try_from(round.requests.len()).unwrap_or(u32::MAX);
            let end = round.base.saturating_add(length);
            if call.step >= round.base && call.step < end {
                round.filed = round.filed.max(call.step - round.base + 1);
                return;
            }
        }
    }

    /// How many responses this turn has seen.
    #[must_use]
    pub const fn rounds(&self) -> usize {
        self.rounds.len()
    }

    /// How many calls every round before this one asked for.
    #[must_use]
    pub fn total_calls(&self) -> u32 {
        self.rounds.iter().fold(0_u32, |total, round| {
            total.saturating_add(u32::try_from(round.requests.len()).unwrap_or(u32::MAX))
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnfiledReply {
    pub turn: TurnId,
    pub content: String,
    pub tokens: u64,
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
    /// The depth of this branch's `branch.created` event.
    pub origin: u64,
    pub depth: u64,
    pub tip: Option<EventId>,
    pub messages: Vec<Message>,
    pub turns: Vec<Turn>,
    pub pending: BTreeMap<CallId, String>,
    /// The one approval that is waiting for an answer on this branch.
    ///
    /// One at a time, because a stack of questions is not what an operator can
    /// answer well, and the loop asks for one call at a time anyway.
    pub approval: Option<Approval>,
    /// Tools whose network risk this branch has already allowed.
    pub granted: BTreeSet<String>,
    /// What a person answered about each call, whether they said yes or no.
    pub decided: BTreeMap<CallId, bool>,
    pub pending_command: Option<Command>,
    pub pending_model: Option<PendingModel>,
    pub unfiled: Option<UnfiledReply>,
    /// What each turn asked the tools to do, in the order it asked.
    pub asked: BTreeMap<TurnId, Asked>,
    pub cache: Cache,
    pub failures: u32,
}

/// Reopens a turn once the last call it asked for has an outcome.
///
/// The bound exists because a model that keeps asking for tools would otherwise
/// keep the loop busy forever. When it is reached the turn stops asking and the
/// results stay in the log, which is a state an operator can read and act on.
fn reopen(
    branch: &mut Branch,
    call: CallId,
) {
    if !branch.pending.is_empty() || branch.archived {
        return;
    }
    let rounds = branch.asked.get(&call.turn).map_or(0, Asked::rounds);
    if rounds >= MAX_TOOL_ROUNDS {
        return;
    }
    if branch.pending_model.is_some() {
        return;
    }
    branch.pending_model = Some(PendingModel {
        turn: call.turn,
        attempts: 0,
        due: None,
    });
}

/// Files one response, its calls, and the step each call starts at.
fn respond(
    branch: &mut Branch,
    event: &Event,
    base: Option<u32>,
    calls: &[ToolRequest],
    turn: TurnId,
    output: &str,
    output_tokens: u64,
) -> Result<(), FoldError> {
    if !branch.has_turn(turn) {
        return Err(FoldError::UnknownTurn(turn));
    }
    branch.pending_model = None;
    if calls.is_empty() {
        // An answer with no words in it is not an answer, and an answer with
        // words is filed by a decision so a restart can repeat it.
        if !output.is_empty() {
            branch.unfiled = Some(UnfiledReply {
                turn,
                content: String::from(output),
                tokens: output_tokens,
            });
        }
        return Ok(());
    }
    // A response that asked for tools is part of the conversation as it stands:
    // the provider needs it in front of the results, with the ids those results
    // will answer. Nothing else records it, so the fold puts it here.
    let base = base.unwrap_or(0);
    branch
        .asked
        .entry(turn)
        .or_default()
        .push(base, calls.to_vec());
    branch.messages.push(Message {
        role: Role::Assistant,
        call: None,
        depth: event.depth,
        calls: calls
            .iter()
            .enumerate()
            .map(|(step, request)| AskedCall {
                call: CallId {
                    turn,
                    step: base.saturating_add(u32::try_from(step).unwrap_or(u32::MAX)),
                },
                args: request.args.clone(),
                tool: request.tool.clone(),
            })
            .collect(),
        content: String::from(output),
        tokens: Some(output_tokens),
        turn: Some(turn),
    });
    Ok(())
}

/// Records a call the process could not finish, and tells the model.
fn fail(
    branch: &mut Branch,
    event: &Event,
    call: &CallId,
    class: ErrorClass,
    may_have_happened: bool,
) -> Result<(), FoldError> {
    if branch.pending.remove(call).is_none() {
        return Err(FoldError::UnknownCall(*call));
    }
    let mut content = alloc::format!("tool call {call} failed: {}", class.as_str());
    if may_have_happened {
        content.push_str(" (the effect may have happened)");
    }
    branch.messages.push(Message {
        role: Role::Tool,
        call: Some(*call),
        depth: event.depth,
        calls: Vec::new(),
        content,
        tokens: None,
        turn: Some(call.turn),
    });
    reopen(branch, *call);
    Ok(())
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
            origin: event.depth,
            depth: event.depth,
            tip: Some(event.id()),
            messages: Vec::new(),
            turns: Vec::new(),
            pending: BTreeMap::new(),
            approval: None,
            granted: BTreeSet::new(),
            decided: BTreeMap::new(),
            pending_command: None,
            pending_model: None,
            unfiled: None,
            asked: BTreeMap::new(),
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

    /// The messages this branch inherited, followed by its own.
    ///
    /// A branch's transcript is what its ancestors wrote before its origin, and
    /// then what it wrote itself. Inherited messages stay on the branch that
    /// wrote them: a reader sees them without the log holding a second copy.
    #[must_use]
    pub fn transcript(
        &self,
        branch: BranchId,
    ) -> Vec<&Message> {
        let mut frames: Vec<(&Branch, Option<u64>)> = Vec::new();
        let mut seen = BTreeSet::new();
        let mut current = Some(branch);
        let mut cut = None;
        while let Some(id) = current {
            // A branch names its parent without the fold proving the parent is
            // not below it, so a cycle stops the walk instead of the process.
            if !seen.insert(id) {
                break;
            }
            let Some(facts) = self.branches.get(&id) else {
                break;
            };
            frames.push((facts, cut));
            cut = Some(facts.origin.saturating_sub(1));
            current = facts.parent;
        }
        let mut messages = Vec::new();
        for (facts, cut) in frames.iter().rev() {
            let kept = facts
                .messages
                .iter()
                .filter(|message| cut.is_none_or(|cut| message.depth <= cut));
            messages.extend(kept);
        }
        messages
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
                // The asking is a fact about the transcript as much as the
                // answering is. Projecting it here keeps the order right without
                // a second event, and a later turn then sees what was asked.
                branch.messages.push(Message {
                    role: Role::User,
                    call: None,
                    depth: event.depth,
                    calls: Vec::new(),
                    content: input.clone(),
                    tokens: None,
                    turn: Some(*turn),
                });
                branch.pending_command = None;
                branch.pending_model = Some(PendingModel {
                    turn: *turn,
                    attempts: 0,
                    due: None,
                });
            },
            EventKind::CommandReceived { command, text, .. } => {
                branch.pending_command = Some(Command {
                    event: event.id(),
                    name: command.clone(),
                    text: text.clone(),
                });
            },
            EventKind::ModelRequested { turn, .. } => {
                if !branch.has_turn(*turn) {
                    return Err(FoldError::UnknownTurn(*turn));
                }
                if let Some(pending) = &mut branch.pending_model
                    && pending.turn == *turn
                {
                    pending.attempts = pending.attempts.saturating_add(1);
                    pending.due = None;
                }
            },
            EventKind::ModelResponded {
                base,
                calls,
                turn,
                output,
                output_tokens,
                ..
            } => respond(branch, event, *base, calls, *turn, output, *output_tokens)?,
            EventKind::ModelFailed {
                turn, retryable, ..
            } => {
                if !branch.has_turn(*turn) {
                    return Err(FoldError::UnknownTurn(*turn));
                }
                branch.failures = branch.failures.saturating_add(1);
                if !retryable {
                    branch.pending_model = None;
                }
            },
            EventKind::ScheduleArmed { turn, due, .. } => {
                if !branch.has_turn(*turn) {
                    return Err(FoldError::UnknownTurn(*turn));
                }
                if let Some(pending) = &mut branch.pending_model
                    && pending.turn == *turn
                {
                    pending.due = Some(*due);
                }
            },
            EventKind::ModelCacheObserved {
                cached,
                reported,
                uncached,
                ..
            } => {
                // Saturating, like the failure count: a pure fold must not panic
                // on a number a log can carry.
                branch.cache.cached = branch.cache.cached.saturating_add(*cached);
                branch.cache.uncached = branch.cache.uncached.saturating_add(*uncached);
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
            } => {
                branch.messages.push(Message {
                    role: *role,
                    call: None,
                    depth: event.depth,
                    calls: Vec::new(),
                    content: content.clone(),
                    tokens: Some(*tokens),
                    turn: event.correlation,
                });
                if branch
                    .unfiled
                    .as_ref()
                    .is_some_and(|unfiled| event.correlation == Some(unfiled.turn))
                {
                    branch.unfiled = None;
                }
            },
            EventKind::ApprovalRequested {
                call,
                expires,
                risk,
                tool,
            } => {
                if !branch.pending.contains_key(call) {
                    return Err(FoldError::UnknownCall(*call));
                }
                branch.approval = Some(Approval {
                    call: *call,
                    expires: *expires,
                    risk: *risk,
                    tool: tool.clone(),
                });
            },
            EventKind::ApprovalDecided { allow, call, .. } => {
                let Some(approval) = branch.approval.as_ref() else {
                    return Err(FoldError::UnknownCall(*call));
                };
                if approval.call != *call {
                    return Err(FoldError::UnknownCall(*call));
                }
                if *allow && approval.risk.is_remembered() {
                    branch.granted.insert(approval.tool.clone());
                }
                branch.decided.insert(*call, *allow);
                branch.approval = None;
                branch.pending_command = None;
            },
            EventKind::ToolIntent { call, tool, .. } => {
                if branch.pending.contains_key(call) {
                    return Err(FoldError::RepeatedCall(*call));
                }
                branch.pending.insert(*call, tool.clone());
                if let Some(asked) = branch.asked.get_mut(&call.turn) {
                    asked.mark_filed(*call);
                }
            },
            EventKind::ToolResult { call, output, .. } => {
                if branch.pending.remove(call).is_none() {
                    return Err(FoldError::UnknownCall(*call));
                }
                // The model cannot answer a result it never sees, so the output is
                // part of the transcript, addressed to the call that produced it.
                branch.messages.push(Message {
                    role: Role::Tool,
                    call: Some(*call),
                    depth: event.depth,
                    calls: Vec::new(),
                    content: output.clone(),
                    tokens: None,
                    turn: Some(call.turn),
                });
                reopen(branch, *call);
            },
            EventKind::ToolFailed {
                call,
                class,
                may_have_happened,
            } => fail(branch, event, call, *class, *may_have_happened)?,
            EventKind::BranchCreated { .. }
            | EventKind::InstanceStarted { .. }
            | EventKind::InstanceStopping { .. }
            | EventKind::InstanceRecovered { .. }
            | EventKind::InstanceClockSkew { .. }
            | EventKind::Unknown { .. } => {},
        }
        // Moving this above the match would let a rejected event advance the tip.
        branch.depth = event.depth;
        branch.tip = Some(event.id());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{FoldError, State};
    use crate::event::{ErrorClass, Event, EventKind, FinishReason, Risk, Role, ToolRequest};
    use crate::ids::{Actor, BranchId, CallId, EventId, PrefixHash, TurnId};
    use crate::time::Timestamp;
    use alloc::collections::BTreeMap;
    use alloc::string::{String, ToString};
    use alloc::vec;
    use alloc::vec::Vec;
    use proptest::prelude::*;

    const BRANCH: [u8; 8] = [1; 8];
    const OTHER: [u8; 8] = [2; 8];
    const TURN: [u8; 8] = [3; 8];
    const LEAF: [u8; 8] = [4; 8];

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

    fn appended(content: &str) -> EventKind {
        EventKind::MessageAppended {
            content: content.to_string(),
            role: Role::Assistant,
            tokens: 1,
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

    fn transcript(
        state: &State,
        branch: BranchId,
    ) -> Vec<String> {
        state
            .transcript(branch)
            .into_iter()
            .map(|message| message.content.clone())
            .collect()
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
        assert_eq!(branch.origin, 4);
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
    fn a_rejected_event_leaves_the_state_it_started_with() {
        let (mut chain, mut state) = with_branch();
        let first = chain.push(EventKind::TurnRequested {
            input: "hello".to_string(),
            turn: turn(),
        });
        state.apply(&first).expect("folds");
        let before = state.clone();
        let duplicate = chain.push(EventKind::TurnRequested {
            input: "again".to_string(),
            turn: turn(),
        });
        assert_eq!(
            state.apply(&duplicate),
            Err(FoldError::DuplicateTurn(turn()))
        );
        assert_eq!(state, before);
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
            base: None,
            calls: alloc::vec::Vec::new(),
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

    fn asked(state: &State) -> (usize, u32) {
        let branch = state.branch(BranchId::from_bytes(BRANCH)).expect("exists");
        let Some(asked) = branch.asked.get(&turn()) else {
            return (0, 0);
        };
        let requests = asked.rounds.iter().map(|round| round.requests.len()).sum();
        let filed = asked.rounds.iter().map(|round| round.filed).sum();
        (requests, filed)
    }

    #[test]
    fn a_response_that_only_asks_for_a_tool_files_no_reply() {
        let (mut chain, mut state) = with_branch();
        let requested = chain.push(EventKind::TurnRequested {
            input: "hello".to_string(),
            turn: turn(),
        });
        state.apply(&requested).expect("folds");
        let responded = chain.push(EventKind::ModelResponded {
            base: None,
            calls: alloc::vec![ToolRequest {
                args: "{}".to_string(),
                tool: "clock".to_string(),
            }],
            finish: FinishReason::ToolCalls,
            input_tokens: 1,
            output: String::new(),
            output_tokens: 0,
            request: "r".to_string(),
            turn: turn(),
        });
        state.apply(&responded).expect("folds");
        let branch = state.branch(BranchId::from_bytes(BRANCH)).expect("exists");
        assert!(branch.unfiled.is_none(), "an empty answer is not an answer");
        assert_eq!(asked(&state), (1, 0));
    }

    #[test]
    fn the_same_call_filed_twice_is_an_error() {
        let (mut chain, mut state) = with_branch();
        let requested = chain.push(EventKind::TurnRequested {
            input: "hello".to_string(),
            turn: turn(),
        });
        state.apply(&requested).expect("folds");
        let intent = chain.push(EventKind::ToolIntent {
            args: "{}".to_string(),
            call: CallId {
                turn: turn(),
                step: 0,
            },
            key: "k".to_string(),
            tool: "clock".to_string(),
            env: alloc::vec::Vec::new(),
        });
        state.apply(&intent).expect("folds");
        // A second writer that filed the same step would arrive as its own event,
        // with its own hash, and the fold is the only place that can refuse it.
        let again = chain.push(EventKind::ToolIntent {
            args: "{}".to_string(),
            call: CallId {
                turn: turn(),
                step: 0,
            },
            key: "k2".to_string(),
            tool: "clock".to_string(),
            env: alloc::vec::Vec::new(),
        });
        assert_eq!(
            state.apply(&again),
            Err(FoldError::RepeatedCall(CallId {
                turn: turn(),
                step: 0,
            }))
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
            detail: None,
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
        assert_eq!(branch.messages[0].tokens, Some(3));
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
    fn cache_totals_stop_at_the_largest_count() {
        let (mut chain, mut state) = with_branch();
        for (cached, uncached) in [(u64::MAX, u64::MAX), (1, 1)] {
            let observed = chain.push(EventKind::ModelCacheObserved {
                cached,
                prefix: PrefixHash::from_bytes([0; 32]),
                reported: true,
                turn: turn(),
                uncached,
            });
            state.apply(&observed).expect("folds");
        }
        let cache = state
            .branch(BranchId::from_bytes(BRANCH))
            .expect("exists")
            .cache;
        assert_eq!(cache.cached, u64::MAX);
        assert_eq!(cache.uncached, u64::MAX);
        assert_eq!(cache.reported, 2);
    }

    #[test]
    fn an_intent_is_pending_until_its_result() {
        let (mut chain, mut state) = with_branch();
        let intent = chain.push(EventKind::ToolIntent {
            args: "{}".to_string(),
            call: call(),
            key: "k".to_string(),
            tool: "shell".to_string(),
            env: alloc::vec::Vec::new(),
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
            output: String::new(),
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
            env: alloc::vec::Vec::new(),
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
            output: String::new(),
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
    #[test]
    fn a_turn_with_one_call_still_owing_does_not_reopen() {
        let (mut chain, mut state) = with_branch();
        let requested = chain.push(EventKind::TurnRequested {
            input: "hello".to_string(),
            turn: turn(),
        });
        state.apply(&requested).expect("folds");
        let responded = chain.push(EventKind::ModelResponded {
            base: None,
            calls: alloc::vec![
                ToolRequest {
                    args: "{}".to_string(),
                    tool: "clock".to_string(),
                },
                ToolRequest {
                    args: "{}".to_string(),
                    tool: "shell".to_string(),
                },
            ],
            finish: FinishReason::ToolCalls,
            input_tokens: 1,
            output: String::new(),
            output_tokens: 0,
            request: "r".to_string(),
            turn: turn(),
        });
        state.apply(&responded).expect("folds");
        for step in 0..2 {
            let intent = chain.push(EventKind::ToolIntent {
                args: "{}".to_string(),
                call: CallId { turn: turn(), step },
                key: alloc::format!("k{step}"),
                tool: "clock".to_string(),
                env: alloc::vec::Vec::new(),
            });
            state.apply(&intent).expect("folds");
        }
        let first = chain.push(EventKind::ToolResult {
            call: CallId {
                turn: turn(),
                step: 0,
            },
            duration_ms: 1,
            output: "noon".to_string(),
            status: 0,
            truncated: false,
        });
        state.apply(&first).expect("folds");
        let branch = state.branch(BranchId::from_bytes(BRANCH)).expect("exists");
        assert!(
            branch.pending_model.is_none(),
            "the second call has not run yet, so there is nothing to ask"
        );
    }

    #[test]
    fn an_archived_branch_does_not_reopen_a_turn() {
        let (mut chain, mut state) = with_branch();
        let requested = chain.push(EventKind::TurnRequested {
            input: "hello".to_string(),
            turn: turn(),
        });
        state.apply(&requested).expect("folds");
        let responded = chain.push(EventKind::ModelResponded {
            base: None,
            calls: alloc::vec![ToolRequest {
                args: "{}".to_string(),
                tool: "clock".to_string(),
            }],
            finish: FinishReason::ToolCalls,
            input_tokens: 1,
            output: String::new(),
            output_tokens: 0,
            request: "r".to_string(),
            turn: turn(),
        });
        state.apply(&responded).expect("folds");
        let intent = chain.push(EventKind::ToolIntent {
            args: "{}".to_string(),
            call: CallId {
                turn: turn(),
                step: 0,
            },
            key: "k".to_string(),
            tool: "clock".to_string(),
            env: alloc::vec::Vec::new(),
        });
        state.apply(&intent).expect("folds");
        let archived = chain.push(EventKind::BranchArchived { reason: None });
        state.apply(&archived).expect("folds");
        let result = chain.push(EventKind::ToolResult {
            call: CallId {
                turn: turn(),
                step: 0,
            },
            duration_ms: 1,
            output: String::new(),
            status: 0,
            truncated: false,
        });
        state.apply(&result).expect("folds");
        assert!(
            state
                .branch(BranchId::from_bytes(BRANCH))
                .expect("exists")
                .pending_model
                .is_none(),
            "an archived branch is not asked again"
        );
    }

    /// A turn with one call whose question is open.
    fn armed_question(risk: Risk) -> (Chain, State) {
        let (mut chain, mut state) = with_branch();
        let requested = chain.push(EventKind::TurnRequested {
            input: "hello".to_string(),
            turn: turn(),
        });
        state.apply(&requested).expect("folds");
        let responded = chain.push(EventKind::ModelResponded {
            base: None,
            calls: alloc::vec![ToolRequest {
                args: "{}".to_string(),
                tool: "clock".to_string(),
            }],
            finish: FinishReason::ToolCalls,
            input_tokens: 1,
            output: String::new(),
            output_tokens: 0,
            request: "r".to_string(),
            turn: turn(),
        });
        state.apply(&responded).expect("folds");
        let intent = chain.push(EventKind::ToolIntent {
            args: "{}".to_string(),
            call: CallId {
                turn: turn(),
                step: 0,
            },
            key: "k".to_string(),
            tool: "clock".to_string(),
            env: alloc::vec::Vec::new(),
        });
        state.apply(&intent).expect("folds");
        let asked = chain.push(EventKind::ApprovalRequested {
            call: CallId {
                turn: turn(),
                step: 0,
            },
            expires: Timestamp::new(1_790_000_000, 0).expect("valid"),
            risk,
            tool: "clock".to_string(),
        });
        state.apply(&asked).expect("folds");
        (chain, state)
    }

    #[test]
    fn an_answer_to_a_different_question_is_an_error() {
        let (mut chain, mut state) = armed_question(Risk::Destructive);
        let answered = chain.push(EventKind::ApprovalDecided {
            allow: true,
            call: CallId {
                turn: turn(),
                step: 7,
            },
            note: None,
        });
        assert_eq!(
            state.apply(&answered),
            Err(FoldError::UnknownCall(CallId {
                turn: turn(),
                step: 7
            }))
        );
    }

    #[test]
    fn a_yes_for_a_network_tool_is_remembered_for_the_branch() {
        let (mut chain, mut state) = armed_question(Risk::Network);
        let answered = chain.push(EventKind::ApprovalDecided {
            allow: true,
            call: CallId {
                turn: turn(),
                step: 0,
            },
            note: None,
        });
        state.apply(&answered).expect("folds");
        let branch = state.branch(BranchId::from_bytes(BRANCH)).expect("exists");
        assert!(branch.granted.contains("clock"));
        assert_eq!(
            branch.decided.get(&CallId {
                turn: turn(),
                step: 0
            }),
            Some(&true)
        );
    }

    #[test]
    fn a_yes_for_a_destructive_tool_is_not_remembered() {
        let (mut chain, mut state) = armed_question(Risk::Destructive);
        let answered = chain.push(EventKind::ApprovalDecided {
            allow: true,
            call: CallId {
                turn: turn(),
                step: 0,
            },
            note: None,
        });
        state.apply(&answered).expect("folds");
        let branch = state.branch(BranchId::from_bytes(BRANCH)).expect("exists");
        assert!(branch.granted.is_empty());
    }

    #[test]
    fn a_no_is_recorded_as_a_no() {
        let (mut chain, mut state) = armed_question(Risk::Network);
        let answered = chain.push(EventKind::ApprovalDecided {
            allow: false,
            call: CallId {
                turn: turn(),
                step: 0,
            },
            note: None,
        });
        state.apply(&answered).expect("folds");
        let branch = state.branch(BranchId::from_bytes(BRANCH)).expect("exists");
        assert!(branch.granted.is_empty(), "a no grants nothing");
        assert_eq!(
            branch.decided.get(&CallId {
                turn: turn(),
                step: 0
            }),
            Some(&false)
        );
    }

    #[test]
    fn a_question_about_an_unknown_call_is_an_error() {
        let (mut chain, mut state) = with_branch();
        let asked = chain.push(EventKind::ApprovalRequested {
            call: CallId {
                turn: turn(),
                step: 0,
            },
            expires: Timestamp::new(1_790_000_000, 0).expect("valid"),
            risk: Risk::Destructive,
            tool: "shell".to_string(),
        });
        assert_eq!(
            state.apply(&asked),
            Err(FoldError::UnknownCall(CallId {
                turn: turn(),
                step: 0
            }))
        );
    }

    #[test]
    fn an_answer_to_a_question_nobody_asked_is_an_error() {
        let (mut chain, mut state) = with_branch();
        let decided = chain.push(EventKind::ApprovalDecided {
            allow: true,
            call: CallId {
                turn: turn(),
                step: 0,
            },
            note: None,
        });
        assert_eq!(
            state.apply(&decided),
            Err(FoldError::UnknownCall(CallId {
                turn: turn(),
                step: 0
            }))
        );
    }

    #[test]
    fn a_tool_result_enters_the_transcript_and_reopens_the_turn() {
        let (mut chain, mut state) = with_branch();
        let requested = chain.push(EventKind::TurnRequested {
            input: "hello".to_string(),
            turn: turn(),
        });
        state.apply(&requested).expect("folds");
        let responded = chain.push(EventKind::ModelResponded {
            base: None,
            calls: alloc::vec![ToolRequest {
                args: "{}".to_string(),
                tool: "clock".to_string(),
            }],
            finish: FinishReason::ToolCalls,
            input_tokens: 1,
            output: String::new(),
            output_tokens: 0,
            request: "r".to_string(),
            turn: turn(),
        });
        state.apply(&responded).expect("folds");
        let intent = chain.push(EventKind::ToolIntent {
            args: "{}".to_string(),
            call: CallId {
                turn: turn(),
                step: 0,
            },
            key: "k".to_string(),
            tool: "clock".to_string(),
            env: alloc::vec::Vec::new(),
        });
        state.apply(&intent).expect("folds");

        let branch = state.branch(BranchId::from_bytes(BRANCH)).expect("exists");
        assert_eq!(
            branch.messages.last().expect("a message").call,
            None,
            "the asking is in the transcript as it stands"
        );
        let asking = branch
            .messages
            .iter()
            .rev()
            .find(|message| !message.calls.is_empty())
            .expect("the answer that asked");
        assert_eq!(asking.calls[0].tool, "clock");
        assert_eq!(asking.calls[0].call.step, 0);

        let result = chain.push(EventKind::ToolResult {
            call: CallId {
                turn: turn(),
                step: 0,
            },
            duration_ms: 1,
            output: "noon".to_string(),
            status: 0,
            truncated: false,
        });
        state.apply(&result).expect("folds");

        let branch = state.branch(BranchId::from_bytes(BRANCH)).expect("exists");
        let last = branch.messages.last().expect("a message");
        assert_eq!(last.role, Role::Tool);
        assert_eq!(last.content, "noon");
        assert_eq!(
            last.call,
            Some(CallId {
                turn: turn(),
                step: 0
            })
        );
        assert!(
            branch.pending_model.is_some(),
            "the model has not answered the result yet"
        );
    }

    #[test]
    fn a_root_branch_transcript_is_its_own_messages() {
        let (mut chain, mut state) = with_branch();
        let requested = chain.push(EventKind::TurnRequested {
            input: "hello".to_string(),
            turn: turn(),
        });
        state.apply(&requested).expect("folds");
        let written = chain.push(appended("hi"));
        state.apply(&written).expect("folds");
        assert_eq!(
            transcript(&state, BranchId::from_bytes(BRANCH)),
            ["hello", "hi"]
        );
    }

    #[test]
    fn a_one_level_fork_excludes_messages_at_or_after_its_origin() {
        let parent_id = BranchId::from_bytes(BRANCH);
        let child_id = BranchId::from_bytes(OTHER);
        let mut parent = Chain::new(parent_id);
        let mut state = State::default();
        state.apply(&created(&mut parent)).expect("creates");
        let requested = parent.push(EventKind::TurnRequested {
            input: "first".to_string(),
            turn: turn(),
        });
        state.apply(&requested).expect("folds");
        let kept = parent.push(appended("kept"));
        state.apply(&kept).expect("folds");
        let excluded = parent.push(appended("excluded"));
        state.apply(&excluded).expect("folds");

        let mut child = Chain::new(child_id);
        child.depth = 3;
        child.prev = Some(kept.id());
        let created_child = child.push(EventKind::BranchCreated {
            parent: Some(parent_id),
            fork: Some(kept.id()),
            label: None,
        });
        state.apply(&created_child).expect("creates");
        let own = child.push(appended("own"));
        state.apply(&own).expect("folds");

        assert_eq!(transcript(&state, child_id), ["first", "kept", "own"]);
        assert_eq!(
            transcript(&state, parent_id),
            ["first", "kept", "excluded"],
            "the parent's transcript does not change"
        );
    }

    #[test]
    fn a_two_level_fork_inherits_the_prefix_above_both_origins() {
        let root_id = BranchId::from_bytes(BRANCH);
        let middle_id = BranchId::from_bytes(OTHER);
        let leaf_id = BranchId::from_bytes(LEAF);
        let mut root = Chain::new(root_id);
        let mut state = State::default();
        state.apply(&created(&mut root)).expect("creates");
        let root_a = root.push(appended("root-a"));
        state.apply(&root_a).expect("folds");
        let root_b = root.push(appended("root-b"));
        state.apply(&root_b).expect("folds");

        let mut middle = Chain::new(middle_id);
        middle.depth = 3;
        middle.prev = Some(root_b.id());
        let created_middle = middle.push(EventKind::BranchCreated {
            parent: Some(root_id),
            fork: Some(root_b.id()),
            label: None,
        });
        state.apply(&created_middle).expect("creates");
        let middle_message = middle.push(appended("middle"));
        state.apply(&middle_message).expect("folds");

        let mut leaf = Chain::new(leaf_id);
        leaf.depth = 5;
        leaf.prev = Some(middle_message.id());
        let created_leaf = leaf.push(EventKind::BranchCreated {
            parent: Some(middle_id),
            fork: Some(middle_message.id()),
            label: None,
        });
        state.apply(&created_leaf).expect("creates");
        let leaf_message = leaf.push(appended("leaf"));
        state.apply(&leaf_message).expect("folds");

        assert_eq!(
            transcript(&state, middle_id),
            ["root-a", "root-b", "middle"]
        );
        assert_eq!(
            transcript(&state, leaf_id),
            ["root-a", "root-b", "middle", "leaf"]
        );
    }

    #[test]
    fn an_unknown_branch_has_an_empty_transcript() {
        let (_chain, state) = with_branch();
        assert!(
            state.transcript(BranchId::from_bytes(OTHER)).is_empty(),
            "a branch the fold never saw inherits nothing"
        );
        assert!(
            State::default()
                .transcript(BranchId::from_bytes(OTHER))
                .is_empty(),
            "an empty fold has nothing to inherit"
        );
    }

    #[test]
    fn a_parent_cycle_stops_the_walk() {
        let first_id = BranchId::from_bytes(BRANCH);
        let second_id = BranchId::from_bytes(OTHER);
        let mut state = State::default();
        // Creation checks neither that the parent exists nor that it is above,
        // so two branches can name each other.
        let mut first = Chain::new(first_id);
        first.depth = 2;
        state
            .apply(&first.push(EventKind::BranchCreated {
                parent: Some(second_id),
                fork: None,
                label: None,
            }))
            .expect("creates");
        let mut second = Chain::new(second_id);
        state
            .apply(&second.push(EventKind::BranchCreated {
                parent: Some(first_id),
                fork: None,
                label: None,
            }))
            .expect("creates");
        let ancestor = second.push(appended("ancestor"));
        state.apply(&ancestor).expect("folds");

        assert_eq!(transcript(&state, first_id), ["ancestor"]);
    }

    #[test]
    fn each_message_records_the_depth_of_the_event_that_projected_it() {
        let (mut chain, mut state) = with_branch();
        let requested = chain.push(EventKind::TurnRequested {
            input: "hello".to_string(),
            turn: turn(),
        });
        state.apply(&requested).expect("folds");
        let responded = chain.push(EventKind::ModelResponded {
            base: None,
            calls: alloc::vec![ToolRequest {
                args: "{}".to_string(),
                tool: "clock".to_string(),
            }],
            finish: FinishReason::ToolCalls,
            input_tokens: 1,
            output: String::new(),
            output_tokens: 0,
            request: "r".to_string(),
            turn: turn(),
        });
        state.apply(&responded).expect("folds");
        let intent = chain.push(EventKind::ToolIntent {
            args: "{}".to_string(),
            call: call(),
            key: "k".to_string(),
            tool: "clock".to_string(),
            env: alloc::vec::Vec::new(),
        });
        state.apply(&intent).expect("folds");
        let result = chain.push(EventKind::ToolResult {
            call: call(),
            duration_ms: 1,
            output: "noon".to_string(),
            status: 0,
            truncated: false,
        });
        state.apply(&result).expect("folds");
        let written = chain.push(appended("done"));
        state.apply(&written).expect("folds");

        let depths = state
            .branch(BranchId::from_bytes(BRANCH))
            .expect("exists")
            .messages
            .iter()
            .map(|message| (message.depth, message.role))
            .collect::<Vec<_>>();
        assert_eq!(
            depths,
            [
                (1, Role::User),
                (2, Role::Assistant),
                (4, Role::Tool),
                (5, Role::Assistant),
            ]
        );
    }

    #[derive(Debug, Clone)]
    enum Op {
        Append {
            content: String,
            role: Role,
            tokens: u64,
        },
        Turn {
            input: String,
        },
        Respond {
            output: String,
            input_tokens: u64,
            output_tokens: u64,
        },
        Ask {
            calls: Vec<ToolRequest>,
        },
        Intent {
            args: String,
            key: String,
            tool: String,
        },
        Result {
            duration_ms: u64,
            output: String,
            status: i32,
            truncated: bool,
        },
        Fail {
            class: ErrorClass,
            may_have_happened: bool,
        },
        Cache {
            cached: u64,
            uncached: u64,
            reported: bool,
        },
        ModelFailed {
            class: ErrorClass,
            retryable: bool,
        },
        Clock {
            delta_secs: i64,
        },
        Label {
            label: String,
        },
        Archive {
            reason: Option<String>,
        },
        Command {
            command: String,
            text: Option<String>,
        },
    }

    fn role() -> impl Strategy<Value = Role> {
        prop_oneof![
            Just(Role::System),
            Just(Role::User),
            Just(Role::Assistant),
            Just(Role::Tool),
        ]
    }

    fn class() -> impl Strategy<Value = ErrorClass> {
        prop_oneof![
            Just(ErrorClass::Transport),
            Just(ErrorClass::Timeout),
            Just(ErrorClass::RateLimited),
            Just(ErrorClass::ServerError),
            Just(ErrorClass::InvalidRequest),
            Just(ErrorClass::ContextLength),
            Just(ErrorClass::ContentFilter),
            Just(ErrorClass::Cancelled),
            Just(ErrorClass::Unknown),
        ]
    }

    fn op() -> impl Strategy<Value = Op> {
        prop_oneof![
            (any::<String>(), role(), any::<u64>()).prop_map(|(content, role, tokens)| {
                Op::Append {
                    content,
                    role,
                    tokens,
                }
            }),
            any::<String>().prop_map(|input| Op::Turn { input }),
            (any::<String>(), any::<u64>(), any::<u64>()).prop_map(
                |(output, input_tokens, output_tokens)| {
                    Op::Respond {
                        output,
                        input_tokens,
                        output_tokens,
                    }
                }
            ),
            prop::collection::vec((any::<String>(), any::<String>()), 1..4).prop_map(|parts| {
                Op::Ask {
                    calls: parts
                        .into_iter()
                        .map(|(args, tool)| ToolRequest { args, tool })
                        .collect(),
                }
            }),
            (any::<String>(), any::<String>(), any::<String>())
                .prop_map(|(args, key, tool)| { Op::Intent { args, key, tool } }),
            (any::<u64>(), any::<String>(), any::<i32>(), any::<bool>(),).prop_map(
                |(duration_ms, output, status, truncated)| {
                    Op::Result {
                        duration_ms,
                        output,
                        status,
                        truncated,
                    }
                }
            ),
            (class(), any::<bool>()).prop_map(|(class, may_have_happened)| Op::Fail {
                class,
                may_have_happened,
            }),
            (any::<u64>(), any::<u64>(), any::<bool>()).prop_map(|(cached, uncached, reported)| {
                Op::Cache {
                    cached,
                    uncached,
                    reported,
                }
            }),
            (class(), any::<bool>())
                .prop_map(|(class, retryable)| Op::ModelFailed { class, retryable }),
            any::<i64>().prop_map(|delta_secs| Op::Clock { delta_secs }),
            any::<String>().prop_map(|label| Op::Label { label }),
            prop::option::of(any::<String>()).prop_map(|reason| Op::Archive { reason }),
            (any::<String>(), prop::option::of(any::<String>()))
                .prop_map(|(command, text)| Op::Command { command, text }),
        ]
    }

    const GENERATED_TURN: [u8; 8] = [0x5a; 8];

    #[allow(
        clippy::too_many_lines,
        reason = "one flat match over the operations keeps each event's guard next to its construction"
    )]
    fn sequence(operations: Vec<Op>) -> Vec<Event> {
        let mut chain = Chain::new(BranchId::from_bytes(BRANCH));
        let first = TurnId::from_bytes(GENERATED_TURN);
        let mut events = alloc::vec![
            created(&mut chain),
            chain.push(EventKind::TurnRequested {
                input: String::from("first"),
                turn: first,
            }),
        ];
        let mut turns = alloc::vec![first];
        let mut asked: Vec<CallId> = Vec::new();
        let mut pending: Vec<CallId> = Vec::new();
        let mut next_turn = 0_u64;
        let mut next_step = 0_u32;
        for operation in operations {
            let turn = turns
                .last()
                .copied()
                .expect("the sequence always requests its first turn");
            match operation {
                Op::Append {
                    content,
                    role,
                    tokens,
                } => {
                    events.push(chain.push(EventKind::MessageAppended {
                        content,
                        role,
                        tokens,
                    }));
                },
                Op::Turn { input } => {
                    next_turn += 1;
                    let turn = TurnId::from_bytes(next_turn.to_le_bytes());
                    turns.push(turn);
                    events.push(chain.push(EventKind::TurnRequested { input, turn }));
                },
                Op::Respond {
                    output,
                    input_tokens,
                    output_tokens,
                } => {
                    events.push(chain.push(EventKind::ModelResponded {
                        base: None,
                        calls: Vec::new(),
                        finish: FinishReason::Stop,
                        input_tokens,
                        output,
                        output_tokens,
                        request: String::from("request"),
                        turn,
                    }));
                },
                Op::Ask { calls } => {
                    for _ in &calls {
                        next_step += 1;
                        asked.push(CallId {
                            turn,
                            step: next_step,
                        });
                    }
                    events.push(chain.push(EventKind::ModelResponded {
                        base: None,
                        calls,
                        finish: FinishReason::ToolCalls,
                        input_tokens: 1,
                        output: String::from("calling"),
                        output_tokens: 1,
                        request: String::from("request"),
                        turn,
                    }));
                },
                Op::Intent { args, key, tool } => {
                    let open = asked.iter().position(|call| !pending.contains(call));
                    if let Some(index) = open {
                        let call = asked.remove(index);
                        pending.push(call);
                        events.push(chain.push(EventKind::ToolIntent {
                            args,
                            call,
                            env: Vec::new(),
                            key,
                            tool,
                        }));
                    } else {
                        events.push(chain.push(appended("fallback")));
                    }
                },
                Op::Result {
                    duration_ms,
                    output,
                    status,
                    truncated,
                } => {
                    if let Some(call) = pending.pop() {
                        events.push(chain.push(EventKind::ToolResult {
                            call,
                            duration_ms,
                            output,
                            status,
                            truncated,
                        }));
                    } else {
                        events.push(chain.push(appended("fallback")));
                    }
                },
                Op::Fail {
                    class,
                    may_have_happened,
                } => {
                    if let Some(call) = pending.pop() {
                        events.push(chain.push(EventKind::ToolFailed {
                            call,
                            class,
                            may_have_happened,
                        }));
                    } else {
                        events.push(chain.push(appended("fallback")));
                    }
                },
                Op::Cache {
                    cached,
                    uncached,
                    reported,
                } => {
                    events.push(chain.push(EventKind::ModelCacheObserved {
                        cached,
                        prefix: PrefixHash::from_bytes([0x11; 32]),
                        reported,
                        turn,
                        uncached,
                    }));
                },
                Op::ModelFailed { class, retryable } => {
                    events.push(chain.push(EventKind::ModelFailed {
                        attempt: 1,
                        class,
                        detail: None,
                        retryable,
                        turn,
                    }));
                },
                Op::Clock { delta_secs } => {
                    events.push(chain.push(EventKind::InstanceClockSkew { delta_secs }));
                },
                Op::Label { label } => {
                    events.push(chain.push(EventKind::BranchLabeled { label }));
                },
                Op::Archive { reason } => {
                    events.push(chain.push(EventKind::BranchArchived { reason }));
                },
                Op::Command { command, text } => {
                    events.push(chain.push(EventKind::CommandReceived {
                        command,
                        command_id: String::from("generated"),
                        surface: String::from("test"),
                        text,
                    }));
                },
            }
        }
        events
    }

    proptest! {
        #[test]
        fn a_generated_sequence_folds_to_the_same_state_twice(
            operations in prop::collection::vec(op(), 0..12),
        ) {
            let events = sequence(operations);
            let mut first = State::default();
            let mut second = State::default();
            for event in &events {
                first.apply(event).expect("a generated sequence folds");
                second.apply(event).expect("a generated sequence folds");
            }
            prop_assert_eq!(first, second);
        }

        #[test]
        fn a_generated_sequence_ends_at_the_event_it_folded_last(
            operations in prop::collection::vec(op(), 0..12),
        ) {
            let events = sequence(operations);
            let mut state = State::default();
            for event in &events {
                state.apply(event).expect("a generated sequence folds");
            }
            let branch = state
                .branch(BranchId::from_bytes(BRANCH))
                .expect("the created branch is in the state");
            let last = events.last().expect("the sequence starts with a creation");
            prop_assert_eq!(branch.depth, last.depth);
            prop_assert_eq!(branch.tip, Some(last.id()));
            let depths: Vec<u64> = state
                .transcript(BranchId::from_bytes(BRANCH))
                .into_iter()
                .map(|message| message.depth)
                .collect();
            prop_assert!(depths.windows(2).all(|pair| pair[0] <= pair[1]));
        }

        #[test]
        fn a_rejected_event_leaves_the_state_untouched(
            operations in prop::collection::vec(op(), 0..12),
            contradiction in 0..5_u8,
        ) {
            let events = sequence(operations);
            let mut state = State::default();
            for event in &events {
                state.apply(event).expect("a generated sequence folds");
            }
            let last = events.last().expect("the sequence starts with a creation");
            let after = |kind: EventKind, depth: u64, prev: EventId| Event {
                depth,
                prev: Some(prev),
                kind,
                ..last.clone()
            };
            let (rejected, before) = match contradiction {
                0 => (
                    after(
                        EventKind::TurnRequested {
                            input: String::from("again"),
                            turn: TurnId::from_bytes(GENERATED_TURN),
                        },
                        last.depth + 1,
                        last.id(),
                    ),
                    state.clone(),
                ),
                1 => (
                    after(
                        EventKind::ToolResult {
                            call: CallId {
                                turn: TurnId::from_bytes([0xee; 8]),
                                step: u32::MAX,
                            },
                            duration_ms: 0,
                            output: String::new(),
                            status: 0,
                            truncated: false,
                        },
                        last.depth + 1,
                        last.id(),
                    ),
                    state.clone(),
                ),
                2 => {
                    let mut wrong = *last.id().as_bytes();
                    wrong[0] ^= 0xff;
                    (
                        Event {
                            depth: last.depth + 1,
                            prev: Some(EventId::from_bytes(wrong)),
                            kind: appended("wrong predecessor"),
                            ..last.clone()
                        },
                        state.clone(),
                    )
                },
                3 => (
                    after(appended("skipped a depth"), last.depth + 2, last.id()),
                    state.clone(),
                ),
                _ => {
                    let call = CallId {
                        turn: TurnId::from_bytes(GENERATED_TURN),
                        step: 1_000_000,
                    };
                    let filed = after(
                        EventKind::ToolIntent {
                            args: String::new(),
                            call,
                            env: Vec::new(),
                            key: String::new(),
                            tool: String::from("first"),
                        },
                        last.depth + 1,
                        last.id(),
                    );
                    state.apply(&filed).expect("the first intent folds");
                    let before = state.clone();
                    let repeated = after(
                        EventKind::ToolIntent {
                            args: String::new(),
                            call,
                            env: Vec::new(),
                            key: String::new(),
                            tool: String::from("second"),
                        },
                        filed.depth + 1,
                        filed.id(),
                    );
                    (repeated, before)
                },
            };
            prop_assert!(state.apply(&rejected).is_err());
            prop_assert_eq!(state, before);
        }
    }

    fn asked_rounds(rounds: &[(u32, &[&str])]) -> super::Asked {
        let mut asked = super::Asked::default();
        for (base, tools) in rounds {
            asked.push(
                *base,
                tools
                    .iter()
                    .map(|tool| ToolRequest {
                        args: String::from("{}"),
                        tool: (*tool).to_string(),
                    })
                    .collect(),
            );
        }
        asked
    }

    fn step_call(step: u32) -> CallId {
        CallId { turn: turn(), step }
    }

    #[test]
    fn a_request_is_found_by_the_step_it_carries() {
        let asked = asked_rounds(&[(0, &["a", "b"])]);
        assert_eq!(
            asked
                .request(step_call(0))
                .map(|request| request.tool.as_str()),
            Some("a")
        );
        assert_eq!(
            asked
                .request(step_call(1))
                .map(|request| request.tool.as_str()),
            Some("b")
        );
        assert_eq!(
            asked.request(step_call(2)),
            None,
            "a step no round asked for is nothing"
        );
    }

    #[test]
    fn a_later_round_wins_where_two_rounds_overlap() {
        let asked = asked_rounds(&[(0, &["a", "b"]), (1, &["c"])]);
        assert_eq!(
            asked
                .request(step_call(1))
                .map(|request| request.tool.as_str()),
            Some("c")
        );
        assert_eq!(
            asked
                .request(step_call(0))
                .map(|request| request.tool.as_str()),
            Some("a")
        );
        assert_eq!(asked.request(step_call(2)), None);
    }

    #[test]
    fn a_round_that_starts_past_zero_still_answers_its_own_steps() {
        let asked = asked_rounds(&[(5, &["a", "b", "c"])]);
        assert_eq!(
            asked.request(step_call(4)),
            None,
            "a step before the base is no call"
        );
        assert_eq!(
            asked
                .request(step_call(5))
                .map(|request| request.tool.as_str()),
            Some("a")
        );
        assert_eq!(
            asked
                .request(step_call(7))
                .map(|request| request.tool.as_str()),
            Some("c")
        );
        assert_eq!(asked.request(step_call(8)), None, "the end is exclusive");
    }

    #[test]
    fn filing_a_call_advances_only_its_own_round() {
        let mut asked = asked_rounds(&[(0, &["a", "b"]), (2, &["c"])]);
        asked.mark_filed(step_call(1));
        assert_eq!(
            asked.rounds[0].filed, 2,
            "the first round filed both of its calls"
        );
        assert_eq!(
            asked.rounds[1].filed, 0,
            "a call after the base filed nothing"
        );
        asked.mark_filed(step_call(2));
        assert_eq!(asked.rounds[1].filed, 1);
        asked.mark_filed(step_call(9));
        assert_eq!(
            asked.rounds[1].filed, 1,
            "a step no round asked for filed nothing"
        );
    }

    #[test]
    fn the_rounds_and_their_calls_are_counted() {
        let asked = asked_rounds(&[(0, &["a", "b"]), (2, &["c", "d", "e"])]);
        assert_eq!(asked.rounds(), 2);
        assert_eq!(asked.total_calls(), 5);
        assert_eq!(super::Asked::default().rounds(), 0);
        assert_eq!(super::Asked::default().total_calls(), 0);
    }

    #[test]
    fn the_next_unfiled_call_carries_the_step_the_round_gave_it() {
        let mut asked = asked_rounds(&[(0, &["a"]), (4, &["b", "c"])]);
        let (first, request) = asked
            .next_unfiled(turn())
            .expect("the first round is unfiled");
        assert_eq!(first, step_call(0));
        assert_eq!(request.tool, "a");
        asked.mark_filed(step_call(0));
        let (next, request) = asked
            .next_unfiled(turn())
            .expect("the second round is next");
        assert_eq!(next, step_call(4));
        assert_eq!(request.tool, "b");
        asked.mark_filed(step_call(4));
        asked.mark_filed(step_call(5));
        assert_eq!(asked.next_unfiled(turn()), None);
    }
}
