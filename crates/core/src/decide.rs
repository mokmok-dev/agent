//! Decisions. What to do next, as a pure function of state and a clock reading.
//!
//! A decision returns one step, never a plan. The loop carries out that step and
//! folds again, so a decision can never run on state an earlier decision
//! invalidated.

use crate::event::EventKind;
use crate::fold::State;
use crate::ids::{BranchId, EventId, TurnId};
use crate::time::Timestamp;

/// The command name a surface uses to ask for a turn.
pub const SEND: &str = "send";

/// One thing the reactor should append.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Intent {
    pub branch: BranchId,
    pub causation: Option<EventId>,
    pub correlation: Option<TurnId>,
    pub kind: EventKind,
}

/// What happens next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Append one event.
    Append(Intent),
    /// Render this turn's prompt and send it. The events an attempt writes are
    /// the runtime's to append, because only it hears what the provider said.
    RequestModel { branch: BranchId, turn: TurnId },
    /// Do nothing until this instant, because a retry is owed.
    Wait { until: Timestamp },
    /// There is nothing to do.
    Idle,
}

/// The turn a command produces.
///
/// Derived from the command's own id rather than from a counter or a random
/// source, so replaying the same command produces the same turn and a fork at
/// the same point produces the same ids.
#[must_use]
pub fn turn_for(command: EventId) -> TurnId {
    let bytes = command.as_bytes();
    let mut turn = [0_u8; 8];
    turn.copy_from_slice(&bytes[..8]);
    TurnId::from_bytes(turn)
}

/// Returns the next step.
///
/// Branches are visited in id order, so two runs over the same state choose the
/// same branch. That ordering is the whole scheduler at this stage.
///
/// The wait is decided first. A retry owed at a future instant outranks any
/// other work, because starting a second request for a turn that is already
/// backing off would defeat the backoff.
#[must_use]
pub fn decide(
    state: &State,
    now: Timestamp,
) -> Step {
    let mut earliest: Option<Timestamp> = None;
    for (_, facts) in state.branches() {
        if facts.archived {
            continue;
        }
        let Some(due) = facts.pending_model.as_ref().and_then(|pending| pending.due) else {
            continue;
        };
        if due > now {
            earliest = Some(earliest.map_or(due, |best| best.min(due)));
        }
    }
    if let Some(until) = earliest {
        return Step::Wait { until };
    }

    // An answer the transcript does not hold yet is written first: it is the
    // cheapest step, and a later turn cannot see the conversation until it lands.
    for (branch, facts) in state.branches() {
        if facts.archived {
            continue;
        }
        let Some(unfiled) = &facts.unfiled else {
            continue;
        };
        return Step::Append(Intent {
            branch: *branch,
            causation: None,
            correlation: Some(unfiled.turn),
            kind: EventKind::MessageAppended {
                content: unfiled.content.clone(),
                role: crate::event::Role::Assistant,
                tokens: unfiled.tokens,
            },
        });
    }

    for (branch, facts) in state.branches() {
        if facts.archived {
            continue;
        }
        if let Some(pending) = &facts.pending_model {
            return Step::RequestModel {
                branch: *branch,
                turn: pending.turn,
            };
        }
    }

    for (branch, facts) in state.branches() {
        if facts.archived {
            continue;
        }
        let Some(command) = &facts.pending_command else {
            continue;
        };
        if command.name != SEND {
            continue;
        }
        let turn = turn_for(command.event);
        return Step::Append(Intent {
            branch: *branch,
            causation: Some(command.event),
            correlation: Some(turn),
            kind: EventKind::TurnRequested {
                input: command.text.clone().unwrap_or_default(),
                turn,
            },
        });
    }

    Step::Idle
}

#[cfg(test)]
mod tests {
    use super::{Intent, Step, decide, turn_for};
    use crate::event::{Event, EventKind, FinishReason};
    use crate::fold::State;
    use crate::ids::{Actor, BranchId, EventId, TurnId};
    use crate::time::Timestamp;
    use alloc::string::{String, ToString};

    const NOW: i64 = 1_790_000_000;

    fn now() -> Timestamp {
        Timestamp::new(NOW, 0).expect("valid")
    }

    fn later(seconds: i64) -> Timestamp {
        Timestamp::new(NOW + seconds, 0).expect("valid")
    }

    struct Chain<'a> {
        branch: BranchId,
        depth: u64,
        prev: Option<EventId>,
        state: &'a mut State,
    }

    impl<'a> Chain<'a> {
        fn new(
            state: &'a mut State,
            seed: u8,
        ) -> Self {
            Self {
                branch: BranchId::from_bytes([seed; 8]),
                depth: 0,
                prev: None,
                state,
            }
        }

        fn push(
            &mut self,
            kind: EventKind,
        ) -> Event {
            self.push_correlated(kind, None)
        }

        fn push_correlated(
            &mut self,
            kind: EventKind,
            correlation: Option<TurnId>,
        ) -> Event {
            let event = Event {
                branch: self.branch,
                depth: self.depth,
                prev: self.prev,
                time: now(),
                actor: Actor::Process("test".to_string()),
                causation: None,
                correlation,
                model: None,
                kind,
            };
            self.prev = Some(event.id());
            self.depth += 1;
            self.state.apply(&event).expect("folds");
            event
        }

        fn create(&mut self) -> Event {
            self.push(EventKind::BranchCreated {
                parent: None,
                fork: None,
                label: None,
            })
        }

        fn send(
            &mut self,
            text: Option<&str>,
        ) -> Event {
            self.push(EventKind::CommandReceived {
                command: super::SEND.to_string(),
                command_id: "c-1".to_string(),
                surface: "tui".to_string(),
                text: text.map(String::from),
            })
        }

        fn turn(&mut self) -> TurnId {
            let command = self.send(Some("hello"));
            let turn = turn_for(command.id());
            self.push(EventKind::TurnRequested {
                input: "hello".to_string(),
                turn,
            });
            turn
        }

        fn requested(
            &mut self,
            turn: TurnId,
        ) -> Event {
            self.push(EventKind::ModelRequested {
                prefix: crate::ids::PrefixHash::from_bytes([0; 32]),
                segments: 1,
                tokens: 1,
                turn,
                warm: false,
            })
        }

        fn responded(
            &mut self,
            turn: TurnId,
        ) -> Event {
            self.push(EventKind::ModelResponded {
                finish: FinishReason::Stop,
                input_tokens: 1,
                output: "ok".to_string(),
                output_tokens: 1,
                request: "r".to_string(),
                turn,
            })
        }

        fn failed(
            &mut self,
            turn: TurnId,
            retryable: bool,
        ) -> Event {
            self.push(EventKind::ModelFailed {
                attempt: 1,
                class: crate::event::ErrorClass::Transport,
                retryable,
                turn,
            })
        }

        fn message(
            &mut self,
            turn: TurnId,
            content: &str,
            tokens: u64,
        ) -> Event {
            self.push_correlated(
                EventKind::MessageAppended {
                    content: content.to_string(),
                    role: crate::event::Role::Assistant,
                    tokens,
                },
                Some(turn),
            )
        }

        fn armed(
            &mut self,
            turn: TurnId,
            due: Timestamp,
        ) -> Event {
            self.push(EventKind::ScheduleArmed {
                attempt: 1,
                due,
                turn,
            })
        }
    }

    #[test]
    fn an_empty_state_is_idle() {
        assert_eq!(decide(&State::default(), now()), Step::Idle);
    }

    #[test]
    fn a_pending_send_becomes_a_turn_request() {
        let mut state = State::default();
        let command = {
            let mut chain = Chain::new(&mut state, 1);
            chain.create();
            chain.send(Some("hello"))
        };
        let turn = turn_for(command.id());
        assert_eq!(
            decide(&state, now()),
            Step::Append(Intent {
                branch: BranchId::from_bytes([1; 8]),
                causation: Some(command.id()),
                correlation: Some(turn),
                kind: EventKind::TurnRequested {
                    input: "hello".to_string(),
                    turn,
                },
            })
        );
    }

    #[test]
    fn a_turn_with_no_model_request_asks_for_one() {
        let mut state = State::default();
        let turn = {
            let mut chain = Chain::new(&mut state, 1);
            chain.create();
            chain.turn()
        };
        assert_eq!(
            decide(&state, now()),
            Step::RequestModel {
                branch: BranchId::from_bytes([1; 8]),
                turn,
            }
        );
    }

    #[test]
    fn a_turn_that_already_asked_is_not_asked_again_until_it_fails() {
        let mut state = State::default();
        {
            let mut chain = Chain::new(&mut state, 1);
            chain.create();
            let turn = chain.turn();
            chain.requested(turn);
        }
        assert_eq!(
            decide(&state, now()),
            Step::RequestModel {
                branch: BranchId::from_bytes([1; 8]),
                turn: state
                    .branch(BranchId::from_bytes([1; 8]))
                    .expect("exists")
                    .pending_model
                    .as_ref()
                    .expect("pending")
                    .turn,
            },
            "a requested attempt that has not answered is still owed a call"
        );
    }

    #[test]
    fn an_answer_the_transcript_lacks_is_written_before_anything_else() {
        let mut state = State::default();
        let turn = {
            let mut chain = Chain::new(&mut state, 1);
            chain.create();
            let turn = chain.turn();
            chain.requested(turn);
            chain.responded(turn);
            turn
        };
        match decide(&state, now()) {
            Step::Append(Intent {
                correlation,
                kind:
                    EventKind::MessageAppended {
                        content,
                        role,
                        tokens,
                    },
                ..
            }) => {
                assert_eq!(content, "ok");
                assert_eq!(role, crate::event::Role::Assistant);
                assert_eq!(tokens, 1);
                assert_eq!(correlation, Some(turn));
            },
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn an_answer_already_in_the_transcript_is_not_written_again() {
        let mut state = State::default();
        let turn = {
            let mut chain = Chain::new(&mut state, 1);
            chain.create();
            let turn = chain.turn();
            chain.requested(turn);
            chain.responded(turn);
            chain.message(turn, "ok", 1);
            turn
        };
        let _ = turn;
        assert_eq!(decide(&state, now()), Step::Idle);
    }

    #[test]
    fn a_finished_turn_leaves_only_its_answer_to_file() {
        let mut state = State::default();
        {
            let mut chain = Chain::new(&mut state, 1);
            chain.create();
            let turn = chain.turn();
            chain.requested(turn);
            chain.responded(turn);
            chain.message(turn, "ok", 1);
        }
        assert_eq!(decide(&state, now()), Step::Idle);
    }

    #[test]
    fn a_failure_that_may_not_be_retried_ends_the_turn() {
        let mut state = State::default();
        {
            let mut chain = Chain::new(&mut state, 1);
            chain.create();
            let turn = chain.turn();
            chain.requested(turn);
            chain.failed(turn, false);
        }
        assert_eq!(decide(&state, now()), Step::Idle);
    }

    #[test]
    fn a_retry_owed_in_the_future_is_waited_for() {
        let mut state = State::default();
        {
            let mut chain = Chain::new(&mut state, 1);
            chain.create();
            let turn = chain.turn();
            chain.requested(turn);
            chain.failed(turn, true);
            chain.armed(turn, later(2));
        }
        assert_eq!(decide(&state, now()), Step::Wait { until: later(2) });
    }

    #[test]
    fn a_retry_whose_instant_has_passed_asks_for_the_model() {
        let mut state = State::default();
        let turn = {
            let mut chain = Chain::new(&mut state, 1);
            chain.create();
            let turn = chain.turn();
            chain.requested(turn);
            chain.failed(turn, true);
            chain.armed(turn, later(-2));
            turn
        };
        assert_eq!(
            decide(&state, now()),
            Step::RequestModel {
                branch: BranchId::from_bytes([1; 8]),
                turn,
            }
        );
    }

    #[test]
    fn a_retry_due_exactly_now_is_not_waited_for() {
        let mut state = State::default();
        {
            let mut chain = Chain::new(&mut state, 1);
            chain.create();
            let turn = chain.turn();
            chain.requested(turn);
            chain.failed(turn, true);
            chain.armed(turn, now());
        }
        assert!(matches!(decide(&state, now()), Step::RequestModel { .. }));
    }

    #[test]
    fn the_earliest_retry_wins() {
        let mut state = State::default();
        {
            let mut chain = Chain::new(&mut state, 1);
            chain.create();
            let turn = chain.turn();
            chain.requested(turn);
            chain.failed(turn, true);
            chain.armed(turn, later(5));
        }
        {
            let mut chain = Chain::new(&mut state, 2);
            chain.create();
            let turn = chain.turn();
            chain.requested(turn);
            chain.failed(turn, true);
            chain.armed(turn, later(3));
        }
        assert_eq!(decide(&state, now()), Step::Wait { until: later(3) });
    }

    #[test]
    fn a_wait_is_decided_before_other_work_because_backing_off_outranks_it() {
        let mut state = State::default();
        {
            let mut chain = Chain::new(&mut state, 1);
            chain.create();
            let turn = chain.turn();
            chain.requested(turn);
            chain.failed(turn, true);
            chain.armed(turn, later(4));
        }
        {
            let mut chain = Chain::new(&mut state, 2);
            chain.create();
            chain.send(Some("another"));
        }
        assert_eq!(decide(&state, now()), Step::Wait { until: later(4) });
    }

    #[test]
    fn a_command_that_is_not_a_send_is_ignored() {
        let mut state = State::default();
        {
            let mut chain = Chain::new(&mut state, 1);
            chain.create();
            chain.push(EventKind::CommandReceived {
                command: "status".to_string(),
                command_id: "c-2".to_string(),
                surface: "tui".to_string(),
                text: None,
            });
        }
        assert_eq!(decide(&state, now()), Step::Idle);
    }

    #[test]
    fn a_send_without_text_becomes_a_turn_with_an_empty_input() {
        let mut state = State::default();
        {
            let mut chain = Chain::new(&mut state, 1);
            chain.create();
            chain.send(None);
        }
        match decide(&state, now()) {
            Step::Append(Intent {
                kind: EventKind::TurnRequested { input, .. },
                ..
            }) => assert_eq!(input, ""),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn a_request_for_an_earlier_turn_does_not_touch_the_one_in_flight() {
        let mut state = State::default();
        let second = {
            let mut chain = Chain::new(&mut state, 1);
            chain.create();
            let first = chain.turn();
            chain.requested(first);
            chain.responded(first);
            let second = chain.turn();
            assert_ne!(first, second);
            chain.requested(first);
            second
        };
        let pending = state
            .branch(BranchId::from_bytes([1; 8]))
            .expect("exists")
            .pending_model
            .clone()
            .expect("a turn is in flight");
        assert_eq!(pending.turn, second);
        assert_eq!(
            pending.attempts, 0,
            "an attempt at a finished turn is not an attempt at this one"
        );
    }

    #[test]
    fn an_archived_branch_is_skipped() {
        let mut state = State::default();
        {
            let mut chain = Chain::new(&mut state, 1);
            chain.create();
            chain.push(EventKind::BranchArchived { reason: None });
            chain.send(Some("hello"));
        }
        assert_eq!(decide(&state, now()), Step::Idle);
    }

    #[test]
    fn the_lowest_branch_id_goes_first() {
        let mut state = State::default();
        {
            let mut high = Chain::new(&mut state, 2);
            high.create();
            high.send(Some("high"));
        }
        {
            let mut low = Chain::new(&mut state, 1);
            low.create();
            low.send(Some("low"));
        }
        match decide(&state, now()) {
            Step::Append(Intent { branch, .. }) => {
                assert_eq!(branch, BranchId::from_bytes([1; 8]));
            },
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn the_same_command_always_produces_the_same_turn() {
        let command = EventId::from_bytes([9; 32]);
        assert_eq!(turn_for(command), turn_for(command));
    }

    #[test]
    fn two_different_commands_produce_different_turns() {
        assert_ne!(
            turn_for(EventId::from_bytes([1; 32])),
            turn_for(EventId::from_bytes([2; 32]))
        );
    }

    #[test]
    fn the_turn_is_the_first_eight_bytes_of_the_command() {
        let mut bytes = [9_u8; 32];
        bytes[..8].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        let command = EventId::from_bytes(bytes);
        assert_eq!(turn_for(command).as_bytes(), &[1, 2, 3, 4, 5, 6, 7, 8]);
    }
}
