//! The event envelope and its payloads.
//!
//! An event carries no id field. Its identity is the hash of its canonical
//! bytes, so an event cannot be constructed with an id that disagrees with its
//! content, and `prev` names a predecessor by the same rule.

use alloc::string::String;

use crate::ids::{Actor, BranchId, CallId, EventId, PrefixHash, TurnId};
use crate::taxonomy;
use crate::time::Timestamp;

/// One fact, with everything needed to place it in history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub branch: BranchId,
    pub depth: u64,
    pub prev: Option<EventId>,
    pub time: Timestamp,
    pub actor: Actor,
    pub causation: Option<EventId>,
    pub correlation: Option<TurnId>,
    pub model: Option<String>,
    pub kind: EventKind,
}

impl Event {
    #[must_use]
    pub const fn type_name(&self) -> &str {
        self.kind.type_name()
    }

    #[must_use]
    pub const fn schema_version(&self) -> u32 {
        self.kind.schema_version()
    }

    /// The aggregate this event belongs to, used as the `subject` attribute.
    #[must_use]
    pub fn subject(&self) -> String {
        alloc::format!("branch/{}", self.branch)
    }

    /// The event's identity, which is the hash of its canonical bytes.
    #[must_use]
    pub fn id(&self) -> EventId {
        crate::canonical::event_id(self)
    }
}

/// The conversational role a message carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

impl Role {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
        }
    }
}

/// Why a model stopped producing tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinishReason {
    Stop,
    Length,
    ToolCalls,
    ContentFilter,
    /// A reason this build does not know, kept rather than guessed at.
    Unknown,
}

impl FinishReason {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Stop => "stop",
            Self::Length => "length",
            Self::ToolCalls => "tool_calls",
            Self::ContentFilter => "content_filter",
            Self::Unknown => "unknown",
        }
    }
}

/// A failure, classified so the reactor can decide without parsing a string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    Transport,
    Timeout,
    RateLimited,
    ServerError,
    InvalidRequest,
    ContextLength,
    ContentFilter,
    Cancelled,
    Unknown,
}

impl ErrorClass {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Transport => "transport",
            Self::Timeout => "timeout",
            Self::RateLimited => "rate_limited",
            Self::ServerError => "server_error",
            Self::InvalidRequest => "invalid_request",
            Self::ContextLength => "context_length",
            Self::ContentFilter => "content_filter",
            Self::Cancelled => "cancelled",
            Self::Unknown => "unknown",
        }
    }
}

/// The payload of an event, one variant per type in [`taxonomy::ALL`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventKind {
    InstanceStarted {
        schema: u32,
        version: String,
    },
    InstanceStopping {
        reason: String,
    },
    InstanceRecovered {
        rolled_back: bool,
    },
    InstanceClockSkew {
        delta_secs: i64,
    },
    BranchCreated {
        parent: Option<BranchId>,
        fork: Option<EventId>,
        label: Option<String>,
    },
    BranchLabeled {
        label: String,
    },
    BranchArchived {
        reason: Option<String>,
    },
    CommandReceived {
        command: String,
        command_id: String,
        surface: String,
        text: Option<String>,
    },
    TurnRequested {
        input: String,
        turn: TurnId,
    },
    ModelRequested {
        prefix: PrefixHash,
        segments: u32,
        tokens: u64,
        turn: TurnId,
        warm: bool,
    },
    ModelResponded {
        finish: FinishReason,
        input_tokens: u64,
        output: String,
        output_tokens: u64,
        request: String,
        turn: TurnId,
    },
    ModelCacheObserved {
        cached: u64,
        prefix: PrefixHash,
        reported: bool,
        turn: TurnId,
        uncached: u64,
    },
    ModelFailed {
        attempt: u32,
        class: ErrorClass,
        retryable: bool,
        turn: TurnId,
    },
    /// A retry is owed to this turn at `due`.
    ScheduleArmed {
        attempt: u32,
        due: Timestamp,
        turn: TurnId,
    },
    MessageAppended {
        content: String,
        role: Role,
        tokens: u64,
    },
    ToolIntent {
        args: String,
        call: CallId,
        key: String,
        tool: String,
    },
    ToolResult {
        call: CallId,
        duration_ms: u64,
        status: i32,
        truncated: bool,
    },
    ToolFailed {
        call: CallId,
        class: ErrorClass,
        may_have_happened: bool,
    },
    /// A type this build does not understand. The payload is kept as the raw
    /// JSON text of the `data` attribute so it can be exported unchanged.
    Unknown {
        schema: u32,
        data: String,
    },
}

impl EventKind {
    #[must_use]
    pub const fn type_name(&self) -> &str {
        match self {
            Self::InstanceStarted { .. } => taxonomy::INSTANCE_STARTED,
            Self::InstanceStopping { .. } => taxonomy::INSTANCE_STOPPING,
            Self::InstanceRecovered { .. } => taxonomy::INSTANCE_RECOVERED,
            Self::InstanceClockSkew { .. } => taxonomy::INSTANCE_CLOCK_SKEW,
            Self::BranchCreated { .. } => taxonomy::BRANCH_CREATED,
            Self::BranchLabeled { .. } => taxonomy::BRANCH_LABELED,
            Self::BranchArchived { .. } => taxonomy::BRANCH_ARCHIVED,
            Self::CommandReceived { .. } => taxonomy::COMMAND_RECEIVED,
            Self::TurnRequested { .. } => taxonomy::TURN_REQUESTED,
            Self::ModelRequested { .. } => taxonomy::MODEL_REQUESTED,
            Self::ModelResponded { .. } => taxonomy::MODEL_RESPONDED,
            Self::ModelCacheObserved { .. } => taxonomy::MODEL_CACHE_OBSERVED,
            Self::ModelFailed { .. } => taxonomy::MODEL_FAILED,
            Self::ScheduleArmed { .. } => taxonomy::SCHEDULE_ARMED,
            Self::MessageAppended { .. } => taxonomy::MESSAGE_APPENDED,
            Self::ToolIntent { .. } => taxonomy::TOOL_INTENT,
            Self::ToolResult { .. } => taxonomy::TOOL_RESULT,
            Self::ToolFailed { .. } => taxonomy::TOOL_FAILED,
            Self::Unknown { .. } => "",
        }
    }

    #[must_use]
    pub const fn schema_version(&self) -> u32 {
        match self {
            Self::Unknown { schema, .. } => *schema,
            _ => taxonomy::SCHEMA_VERSION,
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::{ErrorClass, EventKind, FinishReason, Role};
    use crate::taxonomy;

    #[test]
    fn every_kind_names_a_type_the_table_knows() {
        let kinds = sample_kinds();
        assert_eq!(kinds.len(), taxonomy::ALL.len());
        for kind in &kinds {
            assert!(taxonomy::is_known(kind.type_name()), "{}", kind.type_name());
        }
    }

    #[test]
    fn the_known_kinds_cover_every_listed_type() {
        let kinds = sample_kinds();
        let mut named = kinds
            .iter()
            .map(EventKind::type_name)
            .collect::<alloc::vec::Vec<_>>();
        named.sort_unstable();
        let mut listed = taxonomy::ALL.to_vec();
        listed.sort_unstable();
        assert_eq!(named, listed);
    }

    #[test]
    fn a_known_kind_carries_the_starting_schema_version() {
        for kind in &sample_kinds() {
            assert_eq!(kind.schema_version(), taxonomy::SCHEMA_VERSION);
        }
    }

    #[test]
    fn an_unknown_kind_carries_its_own_schema_version() {
        let kind = EventKind::Unknown {
            schema: 7,
            data: alloc::string::String::from("{}"),
        };
        assert_eq!(kind.schema_version(), 7);
        assert_eq!(kind.type_name(), "");
    }

    #[test]
    fn roles_render_as_their_names() {
        assert_eq!(Role::System.as_str(), "system");
        assert_eq!(Role::User.as_str(), "user");
        assert_eq!(Role::Assistant.as_str(), "assistant");
        assert_eq!(Role::Tool.as_str(), "tool");
    }

    #[test]
    fn finish_reasons_render_as_their_names() {
        assert_eq!(FinishReason::Stop.as_str(), "stop");
        assert_eq!(FinishReason::Length.as_str(), "length");
        assert_eq!(FinishReason::ToolCalls.as_str(), "tool_calls");
        assert_eq!(FinishReason::ContentFilter.as_str(), "content_filter");
        assert_eq!(FinishReason::Unknown.as_str(), "unknown");
        assert_ne!(FinishReason::Stop.as_str(), FinishReason::Unknown.as_str());
    }

    #[test]
    fn error_classes_render_as_their_names() {
        assert_eq!(ErrorClass::Transport.as_str(), "transport");
        assert_eq!(ErrorClass::Timeout.as_str(), "timeout");
        assert_eq!(ErrorClass::RateLimited.as_str(), "rate_limited");
        assert_eq!(ErrorClass::ServerError.as_str(), "server_error");
        assert_eq!(ErrorClass::InvalidRequest.as_str(), "invalid_request");
        assert_eq!(ErrorClass::ContextLength.as_str(), "context_length");
        assert_eq!(ErrorClass::ContentFilter.as_str(), "content_filter");
        assert_eq!(ErrorClass::Cancelled.as_str(), "cancelled");
        assert_eq!(ErrorClass::Unknown.as_str(), "unknown");
    }

    pub fn sample_kinds() -> alloc::vec::Vec<EventKind> {
        use crate::ids::{BranchId, CallId, PrefixHash, TurnId};
        use alloc::string::ToString;

        let _root = BranchId::from_bytes([1; 8]);
        let turn = TurnId::from_bytes([2; 8]);
        let call = CallId { turn, step: 0 };
        alloc::vec![
            EventKind::InstanceStarted {
                schema: 1,
                version: "0.0.0".to_string(),
            },
            EventKind::InstanceStopping {
                reason: "signal".to_string(),
            },
            EventKind::InstanceRecovered { rolled_back: true },
            EventKind::InstanceClockSkew { delta_secs: 0 },
            EventKind::BranchCreated {
                parent: None,
                fork: None,
                label: None,
            },
            EventKind::BranchLabeled {
                label: "main".to_string(),
            },
            EventKind::BranchArchived { reason: None },
            EventKind::CommandReceived {
                command: "send".to_string(),
                command_id: "c1".to_string(),
                surface: "tui".to_string(),
                text: Some("hello".to_string()),
            },
            EventKind::TurnRequested {
                input: "hi".to_string(),
                turn,
            },
            EventKind::ModelRequested {
                prefix: PrefixHash::from_bytes([3; 32]),
                segments: 2,
                tokens: 10,
                turn,
                warm: false,
            },
            EventKind::ModelResponded {
                finish: FinishReason::Stop,
                input_tokens: 10,
                output: "hello".to_string(),
                output_tokens: 2,
                request: "req".to_string(),
                turn,
            },
            EventKind::ModelCacheObserved {
                cached: 8,
                prefix: PrefixHash::from_bytes([3; 32]),
                reported: true,
                turn,
                uncached: 2,
            },
            EventKind::ModelFailed {
                attempt: 1,
                class: ErrorClass::Timeout,
                retryable: true,
                turn,
            },
            EventKind::ScheduleArmed {
                attempt: 1,
                due: crate::time::Timestamp::new(1_790_000_000, 0).expect("valid"),
                turn,
            },
            EventKind::MessageAppended {
                content: "hello".to_string(),
                role: Role::Assistant,
                tokens: 2,
            },
            EventKind::ToolIntent {
                args: "{}".to_string(),
                call,
                key: "k".to_string(),
                tool: "shell".to_string(),
            },
            EventKind::ToolResult {
                call,
                duration_ms: 5,
                status: 0,
                truncated: false,
            },
            EventKind::ToolFailed {
                call,
                class: ErrorClass::Unknown,
                may_have_happened: false,
            },
        ]
    }
}
