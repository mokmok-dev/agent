//! Branch ids. A fork's id is derived from what the fork is doing, not drawn
//! from entropy, so a replay of the same inputs names the same branch.

use std::format;

use agent_core::hash::digest;
use agent_core::ids::{BranchId, EventId};
use agent_core::time::Timestamp;

/// The id of a branch forked from `anchor` at `now`.
///
/// The pid and counter are part of the id because two forks of the same event
/// by the same process at the same instant must still be two branches. A
/// duplicate that somehow survives is refused by the store rather than silently
/// merging.
#[must_use]
pub fn branch(
    pid: u32,
    counter: u64,
    anchor: EventId,
    now: Timestamp,
) -> BranchId {
    let text = format!("{pid}:{counter}:{anchor}:{}:{}", now.secs(), now.nanos());
    let digest = digest(text.as_bytes());
    let mut id = [0_u8; 8];
    id.copy_from_slice(&digest.as_bytes()[..8]);
    BranchId::from_bytes(id)
}

#[cfg(test)]
mod tests {
    use super::branch;
    use agent_core::hash::digest;
    use agent_core::ids::EventId;
    use agent_core::time::Timestamp;

    fn anchor(byte: u8) -> EventId {
        EventId::from_bytes([byte; 32])
    }

    fn at(
        secs: i64,
        nanos: u32,
    ) -> Timestamp {
        Timestamp::new(secs, nanos).expect("valid")
    }

    #[test]
    fn the_same_inputs_name_the_same_branch() {
        assert_eq!(
            branch(7, 1, anchor(2), at(10, 20)),
            branch(7, 1, anchor(2), at(10, 20))
        );
    }

    #[test]
    fn each_input_changes_the_name() {
        let named = branch(7, 1, anchor(2), at(10, 20));
        assert_ne!(named, branch(8, 1, anchor(2), at(10, 20)));
        assert_ne!(named, branch(7, 2, anchor(2), at(10, 20)));
        assert_ne!(named, branch(7, 1, anchor(3), at(10, 20)));
        assert_ne!(named, branch(7, 1, anchor(2), at(11, 20)));
        assert_ne!(named, branch(7, 1, anchor(2), at(10, 21)));
    }

    #[test]
    fn the_name_is_the_first_eight_bytes_of_the_documented_digest() {
        let anchor = anchor(2);
        let now = at(10, 20);
        let text = format!("7:1:{anchor}:{}:{}", now.secs(), now.nanos());
        let expected = digest(text.as_bytes());
        let named = branch(7, 1, anchor, now);
        assert_eq!(named.as_bytes(), &expected.as_bytes()[..8]);
    }
}
