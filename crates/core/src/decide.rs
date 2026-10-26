//! Decisions. What to do next, as a pure function of state.
//!
//! The decision returns one intent, never a list, so the loop that carries it
//! out can re-read the state before deciding again. A decision that returned a
//! plan would be deciding on facts a previous step had already changed.

use crate::event::EventKind;
use crate::fold::State;
use crate::ids::{BranchId, EventId, TurnId};

/// The command name a surface uses to ask for a turn.
pub const SEND: &str = "send";

/// One thing the reactor should do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Intent {
    pub branch: BranchId,
    pub causation: Option<EventId>,
    pub correlation: Option<TurnId>,
    pub kind: EventKind,
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

/// Returns the next intent, or nothing when the state asks for no work.
///
/// Branches are visited in id order, so two runs over the same state choose the
/// same branch. That ordering is the whole scheduler at this stage.
#[must_use]
pub fn decide(state: &State) -> Option<Intent> {
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
        return Some(Intent {
            branch: *branch,
            causation: Some(command.event),
            correlation: Some(turn),
            kind: EventKind::TurnRequested {
                input: command.text.clone().unwrap_or_default(),
                turn,
            },
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{decide, turn_for};
    use crate::event::{Event, EventKind};
    use crate::fold::State;
    use crate::ids::{Actor, BranchId, EventId};
    use crate::time::Timestamp;
    use alloc::string::{String, ToString};

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
            let event = Event {
                branch: self.branch,
                depth: self.depth,
                prev: self.prev,
                time: Timestamp::new(1_790_000_000 + i64::try_from(self.depth).expect("small"), 0)
                    .expect("valid"),
                actor: Actor::Process("test".to_string()),
                causation: None,
                correlation: None,
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
    }

    #[test]
    fn an_empty_state_asks_for_nothing() {
        assert_eq!(decide(&State::default()), None);
    }

    #[test]
    fn a_pending_send_becomes_a_turn() {
        let mut state = State::default();
        let command = {
            let mut chain = Chain::new(&mut state, 1);
            chain.create();
            chain.send(Some("hello"))
        };
        let intent = decide(&state).expect("decides");
        assert_eq!(intent.branch, BranchId::from_bytes([1; 8]));
        assert_eq!(
            intent.kind,
            EventKind::TurnRequested {
                input: "hello".to_string(),
                turn: turn_for(command.id()),
            }
        );
    }

    #[test]
    fn a_branch_without_a_command_asks_for_nothing() {
        let mut state = State::default();
        {
            let mut chain = Chain::new(&mut state, 1);
            chain.create();
        }
        assert_eq!(decide(&state), None);
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
        assert_eq!(decide(&state), None);
    }

    #[test]
    fn a_send_without_text_becomes_a_turn_with_an_empty_input() {
        let mut state = State::default();
        {
            let mut chain = Chain::new(&mut state, 1);
            chain.create();
            chain.send(None);
        }
        let intent = decide(&state).expect("decides");
        match intent.kind {
            EventKind::TurnRequested { input, .. } => assert_eq!(input, ""),
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

    #[test]
    fn an_archived_branch_is_skipped() {
        let mut state = State::default();
        {
            let mut chain = Chain::new(&mut state, 1);
            chain.create();
            chain.push(EventKind::BranchArchived { reason: None });
            chain.send(Some("hello"));
        }
        assert_eq!(decide(&state), None);
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
        let intent = decide(&state).expect("decides");
        assert_eq!(intent.branch, BranchId::from_bytes([1; 8]));
    }

    #[test]
    fn a_turn_clears_the_pending_command() {
        let mut state = State::default();
        {
            let mut chain = Chain::new(&mut state, 1);
            chain.create();
            let command = chain.send(Some("hello"));
            chain.push(EventKind::TurnRequested {
                input: "hello".to_string(),
                turn: turn_for(command.id()),
            });
        }
        assert_eq!(decide(&state), None);
    }
}
