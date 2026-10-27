//! Turning one command from the socket into the one reply the client reads.
//!
//! Everything here is decided from the command and the folded state, so the
//! daemon's loop stays a loop: poll, route, answer, step.

use agent_core::time::Timestamp;

use crate::Reactor;
use crate::bus::{self, Refusal, Reply};

/// Carries out one command, and maps its failure to the class the client reads.
///
/// # Errors
///
/// Returns the [`Refusal`] [`bus::refusal`] derives from the reactor's error.
pub fn route(
    reactor: &mut Reactor,
    command: &bus::Command,
    now: Timestamp,
) -> Result<Reply, Refusal> {
    match command {
        bus::Command::Send { branch, text } => reactor
            .command(*branch, "send", text, now)
            .map(|_| Reply::Taken)
            .map_err(|error| bus::refusal(&error)),
        bus::Command::Approve { branch, id } => reactor
            .command(*branch, "approve", id, now)
            .map(|_| Reply::Taken)
            .map_err(|error| bus::refusal(&error)),
        bus::Command::Deny { branch, id } => reactor
            .command(*branch, "deny", id, now)
            .map(|_| Reply::Taken)
            .map_err(|error| bus::refusal(&error)),
        bus::Command::Fork(request) => reactor
            .fork(request, now)
            .map(|forked| Reply::Forked {
                branch: forked.branch,
                depth: forked.depth,
            })
            .map_err(|error| bus::refusal(&error)),
    }
}
