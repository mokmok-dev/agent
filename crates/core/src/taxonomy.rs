//! The event type table.
//!
//! Type strings are fixed here rather than derived from Rust names, because the
//! string is what lands in the log and a rename of a variant must not silently
//! rename history. Every kind in [`crate::event::EventKind`] matches exactly one
//! constant below, and a test walks the whole table.

/// The schema version every known payload starts at.
pub const SCHEMA_VERSION: u32 = 1;

pub const INSTANCE_STARTED: &str = "dev.mokmok.agent.instance.started";
pub const INSTANCE_STOPPING: &str = "dev.mokmok.agent.instance.stopping";
pub const INSTANCE_RECOVERED: &str = "dev.mokmok.agent.instance.recovered";
pub const INSTANCE_CLOCK_SKEW: &str = "dev.mokmok.agent.instance.clock_skew";

pub const BRANCH_CREATED: &str = "dev.mokmok.agent.branch.created";
pub const BRANCH_LABELED: &str = "dev.mokmok.agent.branch.labeled";
pub const BRANCH_ARCHIVED: &str = "dev.mokmok.agent.branch.archived";

pub const COMMAND_RECEIVED: &str = "dev.mokmok.agent.command.received";
pub const TURN_REQUESTED: &str = "dev.mokmok.agent.turn.requested";

pub const MODEL_REQUESTED: &str = "dev.mokmok.agent.model.requested";
pub const MODEL_RESPONDED: &str = "dev.mokmok.agent.model.responded";
pub const MODEL_CACHE_OBSERVED: &str = "dev.mokmok.agent.model.cache.observed";
pub const MODEL_FAILED: &str = "dev.mokmok.agent.model.failed";

pub const MESSAGE_APPENDED: &str = "dev.mokmok.agent.message.appended";

pub const TOOL_INTENT: &str = "dev.mokmok.agent.tool.intent";
pub const TOOL_RESULT: &str = "dev.mokmok.agent.tool.result";
pub const TOOL_FAILED: &str = "dev.mokmok.agent.tool.failed";

/// Every type this build understands, in the order the taxonomy lists them.
pub const ALL: &[&str] = &[
    INSTANCE_STARTED,
    INSTANCE_STOPPING,
    INSTANCE_RECOVERED,
    INSTANCE_CLOCK_SKEW,
    BRANCH_CREATED,
    BRANCH_LABELED,
    BRANCH_ARCHIVED,
    COMMAND_RECEIVED,
    TURN_REQUESTED,
    MODEL_REQUESTED,
    MODEL_RESPONDED,
    MODEL_CACHE_OBSERVED,
    MODEL_FAILED,
    MESSAGE_APPENDED,
    TOOL_INTENT,
    TOOL_RESULT,
    TOOL_FAILED,
];

/// Whether this build can fold a type.
///
/// A consumer that meets an unknown type keeps the bytes and ignores the event
/// rather than failing the fold, so this predicate decides between decoding and
/// preserving, never between reading and refusing.
#[must_use]
pub fn is_known(type_name: &str) -> bool {
    ALL.contains(&type_name)
}

#[cfg(test)]
mod tests {
    use super::{ALL, is_known};

    #[test]
    fn every_listed_type_is_known() {
        for name in ALL {
            assert!(is_known(name), "{name}");
        }
    }

    #[test]
    fn an_unlisted_type_is_unknown() {
        assert!(!is_known("dev.mokmok.agent.nope"));
        assert!(!is_known(""));
    }

    #[test]
    fn the_table_has_no_duplicates() {
        let mut sorted = ALL.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), ALL.len());
    }
}
