//! Fork points. Where a fork starts, read off the lineage the caller loaded.
//!
//! Nothing here reads a clock, a store, or entropy: the same lineage and the
//! same selector always name the same event, so a fork can be replayed.

use alloc::collections::BTreeMap;
use alloc::string::String;

use crate::event::{Event, EventKind};
use crate::ids::{BranchId, CallId, EventId, TurnId};

/// Which event a fork starts from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Selector {
    /// The branch's tip.
    Tip,
    /// One named event.
    At(EventId),
    /// The tip minus this many events.
    Rewind(u32),
    /// The event before this turn's request, so the turn runs again.
    Turn(TurnId),
}

/// Where a fork starts, after any boundary snap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ForkPoint {
    /// The event the child's first event names as its predecessor.
    pub event: EventId,
    /// The branch that owns `event`; the child names it as its parent.
    pub branch: BranchId,
    /// That event's depth.
    pub depth: u64,
    /// The depth the selector chose, when `boundary` moved the point.
    pub snapped_from: Option<u64>,
}

/// Why a selector names no event on the lineage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForkError {
    /// The event is not on the lineage.
    UnknownEvent(EventId),
    /// The turn was never requested on the lineage.
    UnknownTurn(TurnId),
    /// The selector points before the genesis, where no event exists.
    BeforeGenesis,
}

impl core::fmt::Display for ForkError {
    fn fmt(
        &self,
        formatter: &mut core::fmt::Formatter<'_>,
    ) -> core::fmt::Result {
        match self {
            Self::UnknownEvent(id) => write!(formatter, "unknown event {id}"),
            Self::UnknownTurn(id) => write!(formatter, "unknown turn {id}"),
            Self::BeforeGenesis => formatter.write_str("no event before the genesis"),
        }
    }
}

impl core::error::Error for ForkError {}

/// Resolves a selector against a lineage.
///
/// `events` is the lineage ordered by depth: contiguous from the genesis at
/// depth 0 to the tip, whose event is the last element, with the ancestors'
/// branches included.
///
/// With `boundary`, the point snaps back to the greatest event at or before it
/// that starts a prompt segment: a turn request, an appended message, a tool
/// result, or a model response that asks for tools. The prompt cache reuses a
/// prefix only up to one of those.
///
/// # Errors
///
/// Returns [`ForkError::UnknownEvent`] or [`ForkError::UnknownTurn`] when the
/// selector names an event or turn the lineage does not hold, and
/// [`ForkError::BeforeGenesis`] when it points before the first event.
pub fn resolve(
    events: &[Event],
    selector: Selector,
    boundary: bool,
) -> Result<ForkPoint, ForkError> {
    let chosen = index_of(events, selector)?;
    let snapped = if boundary {
        snap(events, chosen)
    } else {
        chosen
    };
    let chosen_event = events.get(chosen).ok_or(ForkError::BeforeGenesis)?;
    let event = events.get(snapped).ok_or(ForkError::BeforeGenesis)?;
    Ok(ForkPoint {
        event: event.id(),
        branch: event.branch,
        depth: event.depth,
        snapped_from: (snapped != chosen).then_some(chosen_event.depth),
    })
}

/// The first call a fork at `depth` would strand, with the tool to name.
///
/// An intent with no result or failure may or may not have happened, so a fork
/// there can repeat an effect. The scan stops at `depth`, and the call id order
/// makes the warning the same on every run.
#[must_use]
pub fn unresolved(
    events: &[Event],
    depth: u64,
) -> Option<(CallId, String)> {
    let mut open: BTreeMap<CallId, String> = BTreeMap::new();
    for event in events.iter().filter(|event| event.depth <= depth) {
        match &event.kind {
            EventKind::ToolIntent { call, tool, .. } => {
                open.insert(*call, tool.clone());
            },
            EventKind::ToolResult { call, .. } | EventKind::ToolFailed { call, .. } => {
                open.remove(call);
            },
            _ => {},
        }
    }
    open.into_iter().next()
}

/// The index the selector names, before any boundary snap.
fn index_of(
    events: &[Event],
    selector: Selector,
) -> Result<usize, ForkError> {
    match selector {
        Selector::Tip => events.len().checked_sub(1).ok_or(ForkError::BeforeGenesis),
        Selector::At(event) => events
            .iter()
            .position(|candidate| candidate.id() == event)
            .ok_or(ForkError::UnknownEvent(event)),
        Selector::Rewind(offset) => {
            let offset = usize::try_from(offset).map_err(|_| ForkError::BeforeGenesis)?;
            events
                .len()
                .checked_sub(1)
                .and_then(|tip| tip.checked_sub(offset))
                .ok_or(ForkError::BeforeGenesis)
        },
        Selector::Turn(turn) => {
            let asked = events
                .iter()
                .position(|candidate| {
                    matches!(
                        &candidate.kind,
                        EventKind::TurnRequested { turn: found, .. } if *found == turn
                    )
                })
                .ok_or(ForkError::UnknownTurn(turn))?;
            asked.checked_sub(1).ok_or(ForkError::BeforeGenesis)
        },
    }
}

/// The greatest index at or below `chosen` that starts a prompt segment.
fn snap(
    events: &[Event],
    chosen: usize,
) -> usize {
    (0..=chosen)
        .rev()
        .find(|index| {
            events
                .get(*index)
                .is_some_and(|event| starts_segment(&event.kind))
        })
        .unwrap_or(0)
}

const fn starts_segment(kind: &EventKind) -> bool {
    match kind {
        EventKind::TurnRequested { .. }
        | EventKind::MessageAppended { .. }
        | EventKind::ToolResult { .. } => true,
        EventKind::ModelResponded { calls, .. } => !calls.is_empty(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::{ForkError, ForkPoint, Selector, resolve, starts_segment, unresolved};
    use crate::event::{ErrorClass, Event, EventKind, FinishReason, Role, ToolRequest};
    use crate::ids::{Actor, BranchId, CallId, EventId, PrefixHash, TurnId};
    use crate::time::Timestamp;
    use alloc::string::{String, ToString};
    use alloc::vec::Vec;

    const TURN: [u8; 8] = [3; 8];

    fn turn() -> TurnId {
        TurnId::from_bytes(TURN)
    }

    fn call(step: u32) -> CallId {
        CallId { turn: turn(), step }
    }

    struct Lineage {
        branch: BranchId,
        depth: u64,
        prev: Option<EventId>,
        time: Timestamp,
        events: Vec<Event>,
    }

    impl Lineage {
        fn new() -> Self {
            Self {
                branch: BranchId::from_bytes([1; 8]),
                depth: 0,
                prev: None,
                time: Timestamp::new(1_790_000_000, 0).expect("valid"),
                events: Vec::new(),
            }
        }

        fn push(
            &mut self,
            kind: EventKind,
        ) -> EventId {
            let event = Event {
                branch: self.branch,
                depth: self.depth,
                prev: self.prev,
                time: self.time,
                actor: Actor::Process("test".to_string()),
                causation: None,
                correlation: None,
                model: None,
                kind,
            };
            let id = event.id();
            self.prev = Some(id);
            self.depth += 1;
            self.events.push(event);
            id
        }

        fn genesis(&mut self) -> EventId {
            self.push(EventKind::BranchCreated {
                parent: None,
                fork: None,
                label: None,
            })
        }

        fn resolve(
            &self,
            selector: Selector,
            boundary: bool,
        ) -> Result<ForkPoint, ForkError> {
            resolve(&self.events, selector, boundary)
        }
    }

    fn turn_requested() -> EventKind {
        EventKind::TurnRequested {
            input: "hello".to_string(),
            turn: turn(),
        }
    }

    fn appended(content: &str) -> EventKind {
        EventKind::MessageAppended {
            content: content.to_string(),
            role: Role::Assistant,
            tokens: 1,
        }
    }

    fn model_requested() -> EventKind {
        EventKind::ModelRequested {
            prefix: PrefixHash::from_bytes([0; 32]),
            segments: 1,
            tokens: 1,
            turn: turn(),
            warm: false,
        }
    }

    fn tool(name: &str) -> ToolRequest {
        ToolRequest {
            args: "{}".to_string(),
            tool: name.to_string(),
        }
    }

    fn responded(calls: Vec<ToolRequest>) -> EventKind {
        let finish = if calls.is_empty() {
            FinishReason::Stop
        } else {
            FinishReason::ToolCalls
        };
        EventKind::ModelResponded {
            base: None,
            calls,
            finish,
            input_tokens: 1,
            output: String::new(),
            output_tokens: 0,
            request: "r".to_string(),
            turn: turn(),
        }
    }

    fn intent(
        step: u32,
        name: &str,
    ) -> EventKind {
        EventKind::ToolIntent {
            args: "{}".to_string(),
            call: call(step),
            env: Vec::new(),
            key: alloc::format!("k{step}"),
            tool: name.to_string(),
        }
    }

    fn result(step: u32) -> EventKind {
        EventKind::ToolResult {
            call: call(step),
            duration_ms: 1,
            output: String::new(),
            status: 0,
            truncated: false,
        }
    }

    fn failed(step: u32) -> EventKind {
        EventKind::ToolFailed {
            call: call(step),
            class: ErrorClass::Unknown,
            may_have_happened: false,
        }
    }

    #[test]
    fn a_lineage_without_events_has_no_tip() {
        let lineage = Lineage::new();
        assert_eq!(
            lineage.resolve(Selector::Tip, false),
            Err(ForkError::BeforeGenesis)
        );
        assert_eq!(
            lineage.resolve(Selector::Rewind(0), false),
            Err(ForkError::BeforeGenesis)
        );
        let unknown = EventId::from_bytes([9; 32]);
        assert_eq!(
            lineage.resolve(Selector::At(unknown), false),
            Err(ForkError::UnknownEvent(unknown))
        );
    }

    #[test]
    fn the_tip_is_the_last_event_of_the_lineage() {
        let mut lineage = Lineage::new();
        lineage.genesis();
        lineage.push(appended("one"));
        let tip = lineage.push(appended("two"));
        let point = lineage.resolve(Selector::Tip, false).expect("resolves");
        assert_eq!(point.event, tip);
        assert_eq!(point.depth, 2);
        assert_eq!(point.snapped_from, None);
    }

    #[test]
    fn at_resolves_the_event_it_names() {
        let mut lineage = Lineage::new();
        lineage.genesis();
        let named = lineage.push(appended("one"));
        lineage.push(appended("two"));
        let point = lineage
            .resolve(Selector::At(named), false)
            .expect("resolves");
        assert_eq!(point.event, named);
        assert_eq!(point.depth, 1);
        assert_eq!(point.snapped_from, None);
    }

    #[test]
    fn an_event_off_the_lineage_is_unknown() {
        let mut lineage = Lineage::new();
        lineage.genesis();
        lineage.push(appended("one"));
        let unknown = EventId::from_bytes([9; 32]);
        assert!(
            lineage.events.iter().all(|event| event.id() != unknown),
            "the fixture must not contain the unknown id"
        );
        assert_eq!(
            lineage.resolve(Selector::At(unknown), false),
            Err(ForkError::UnknownEvent(unknown))
        );
    }

    #[test]
    fn a_fork_point_names_the_branch_that_owns_it() {
        let mut lineage = Lineage::new();
        lineage.genesis();
        let other = BranchId::from_bytes([2; 8]);
        lineage.branch = other;
        lineage.push(appended("child"));

        let point = lineage.resolve(Selector::Tip, false).expect("resolves");
        assert_eq!(point.branch, other);
    }

    #[test]
    fn rewind_counts_back_from_the_tip() {
        let mut lineage = Lineage::new();
        let genesis = lineage.genesis();
        let second = lineage.push(appended("one"));
        let tip = lineage.push(appended("two"));

        let zero = lineage
            .resolve(Selector::Rewind(0), false)
            .expect("resolves");
        assert_eq!((zero.event, zero.depth), (tip, 2));

        let one = lineage
            .resolve(Selector::Rewind(1), false)
            .expect("resolves");
        assert_eq!((one.event, one.depth), (second, 1));

        let first = lineage
            .resolve(Selector::Rewind(2), false)
            .expect("resolves");
        assert_eq!((first.event, first.depth), (genesis, 0));
    }

    #[test]
    fn a_rewind_past_the_genesis_is_an_error() {
        let mut lineage = Lineage::new();
        lineage.genesis();
        lineage.push(appended("one"));
        lineage.push(appended("two"));
        assert_eq!(
            lineage.resolve(Selector::Rewind(3), false),
            Err(ForkError::BeforeGenesis)
        );
        assert_eq!(
            lineage.resolve(Selector::Rewind(u32::MAX), false),
            Err(ForkError::BeforeGenesis)
        );
    }

    #[test]
    fn turn_resolves_the_event_before_its_request() {
        let mut lineage = Lineage::new();
        lineage.genesis();
        let before = lineage.push(appended("hello"));
        lineage.push(turn_requested());
        lineage.push(appended("answered"));
        let point = lineage
            .resolve(Selector::Turn(turn()), false)
            .expect("resolves");
        assert_eq!(point.event, before);
        assert_eq!(point.depth, 1);
    }

    #[test]
    fn a_turn_that_was_never_requested_is_unknown() {
        let mut lineage = Lineage::new();
        lineage.genesis();
        lineage.push(appended("hello"));
        lineage.push(EventKind::TurnRequested {
            input: "other".to_string(),
            turn: TurnId::from_bytes([7; 8]),
        });
        assert_eq!(
            lineage.resolve(Selector::Turn(turn()), false),
            Err(ForkError::UnknownTurn(turn()))
        );
    }

    #[test]
    fn a_turn_at_the_first_index_has_nothing_before_it() {
        let mut lineage = Lineage::new();
        lineage.push(turn_requested());
        assert_eq!(
            lineage.resolve(Selector::Turn(turn()), false),
            Err(ForkError::BeforeGenesis)
        );
    }

    #[test]
    fn a_boundary_snaps_back_to_the_segment_start() {
        let mut lineage = Lineage::new();
        lineage.genesis();
        lineage.push(appended("first"));
        let start = lineage.push(appended("second"));
        lineage.push(model_requested());
        let tip = lineage.push(EventKind::InstanceRecovered { rolled_back: false });

        let plain = lineage.resolve(Selector::Tip, false).expect("resolves");
        assert_eq!(plain.event, tip);
        assert_eq!(plain.depth, 4);
        assert_eq!(plain.snapped_from, None);

        let point = lineage.resolve(Selector::Tip, true).expect("resolves");
        assert_eq!(point.event, start);
        assert_eq!(point.depth, 2);
        assert_eq!(point.snapped_from, Some(4));
    }

    #[test]
    fn a_boundary_keeps_a_point_already_on_a_segment_start() {
        let mut lineage = Lineage::new();
        lineage.genesis();
        let start = lineage.push(turn_requested());
        lineage.push(appended("answered"));
        let point = lineage
            .resolve(Selector::At(start), true)
            .expect("resolves");
        assert_eq!(point.event, start);
        assert_eq!(point.depth, 1);
        assert_eq!(point.snapped_from, None);
    }

    #[test]
    fn a_boundary_without_a_segment_start_snaps_to_the_genesis() {
        let mut lineage = Lineage::new();
        let genesis = lineage.genesis();
        lineage.push(EventKind::BranchLabeled {
            label: "main".to_string(),
        });
        lineage.push(EventKind::InstanceRecovered { rolled_back: false });
        let point = lineage.resolve(Selector::Tip, true).expect("resolves");
        assert_eq!(point.event, genesis);
        assert_eq!(point.depth, 0);
        assert_eq!(point.snapped_from, Some(2));
    }

    #[test]
    fn a_model_response_is_a_segment_start_only_when_it_asks_for_tools() {
        let mut lineage = Lineage::new();
        lineage.genesis();
        let asked = lineage.push(responded(alloc::vec![tool("clock")]));
        let empty = lineage.push(responded(Vec::new()));
        let point = lineage
            .resolve(Selector::At(empty), true)
            .expect("resolves");
        assert_eq!(point.event, asked);
        assert_eq!(point.depth, 1);
        assert_eq!(point.snapped_from, Some(2));
    }

    #[test]
    fn only_the_kinds_that_produce_a_message_start_a_segment() {
        for kind in crate::event::tests::sample_kinds() {
            let expected = matches!(
                &kind,
                EventKind::TurnRequested { .. }
                    | EventKind::MessageAppended { .. }
                    | EventKind::ToolResult { .. }
            );
            assert_eq!(starts_segment(&kind), expected, "{}", kind.type_name());
        }
        assert!(starts_segment(&responded(alloc::vec![tool("clock")])));
        assert!(!starts_segment(&responded(Vec::new())));
    }

    #[test]
    fn every_error_explains_itself() {
        use alloc::format;
        let errors = [
            ForkError::UnknownEvent(EventId::from_bytes([1; 32])),
            ForkError::UnknownTurn(turn()),
            ForkError::BeforeGenesis,
        ];
        for error in errors {
            assert_ne!(format!("{error}"), "");
        }
    }

    #[test]
    fn an_intent_before_the_depth_is_reported() {
        let mut lineage = Lineage::new();
        lineage.genesis();
        lineage.push(intent(0, "shell"));
        lineage.push(appended("waiting"));
        let (id, tool) = unresolved(&lineage.events, 2).expect("reported");
        assert_eq!(id, call(0));
        assert_eq!(tool, "shell");
    }

    #[test]
    fn an_intent_at_the_depth_is_reported() {
        let mut lineage = Lineage::new();
        lineage.genesis();
        lineage.push(intent(0, "shell"));
        assert_eq!(
            unresolved(&lineage.events, 1),
            Some((call(0), "shell".to_string()))
        );
    }

    #[test]
    fn an_intent_after_the_depth_is_not_reported() {
        let mut lineage = Lineage::new();
        lineage.genesis();
        lineage.push(appended("waiting"));
        lineage.push(intent(0, "shell"));
        assert_eq!(unresolved(&lineage.events, 1), None);
    }

    #[test]
    fn a_result_before_the_depth_clears_the_intent() {
        let mut lineage = Lineage::new();
        lineage.genesis();
        lineage.push(intent(0, "shell"));
        lineage.push(result(0));
        assert_eq!(unresolved(&lineage.events, 2), None);
    }

    #[test]
    fn a_failure_before_the_depth_clears_the_intent() {
        let mut lineage = Lineage::new();
        lineage.genesis();
        lineage.push(intent(0, "shell"));
        lineage.push(failed(0));
        assert_eq!(unresolved(&lineage.events, 2), None);
    }

    #[test]
    fn a_result_after_the_depth_does_not_clear_the_intent() {
        let mut lineage = Lineage::new();
        lineage.genesis();
        lineage.push(intent(0, "shell"));
        lineage.push(result(0));
        assert_eq!(
            unresolved(&lineage.events, 1),
            Some((call(0), "shell".to_string()))
        );
    }

    #[test]
    fn the_first_open_call_in_id_order_is_reported() {
        let mut lineage = Lineage::new();
        lineage.genesis();
        lineage.push(intent(1, "second"));
        lineage.push(intent(0, "first"));
        let (id, tool) = unresolved(&lineage.events, 2).expect("reported");
        assert_eq!(id, call(0));
        assert_eq!(tool, "first");
    }
}
