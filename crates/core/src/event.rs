//! The event envelope and its payloads.
//!
//! An event carries no id field. Its identity is the hash of its canonical
//! bytes, so an event cannot be constructed with an id that disagrees with its
//! content, and `prev` names a predecessor by the same rule.

use alloc::string::String;
use alloc::vec::Vec;

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
/// What a tool may touch, and therefore what it takes to run it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Risk {
    /// Reads what the agent can already read. No approval.
    Read,
    /// Writes inside the paths the manifest declares. No approval.
    Write,
    /// Reaches a host the manifest declares. Approved once per branch.
    Network,
    /// Deletes, force pushes, or speaks to a person. Approved every time.
    Destructive,
}

impl Risk {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Network => "network",
            Self::Destructive => "destructive",
        }
    }

    /// Whether running this risk without a person's answer is allowed.
    #[must_use]
    pub const fn needs_approval(self) -> bool {
        matches!(self, Self::Network | Self::Destructive)
    }

    /// Whether one answer is remembered for the rest of the branch.
    #[must_use]
    pub const fn is_remembered(self) -> bool {
        matches!(self, Self::Network)
    }
}

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
        /// The tools the model asked to run, in the order it asked.
        calls: Vec<ToolRequest>,
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
        /// What the failure said about itself, when it said anything.
        ///
        /// The class names the kind of failure and this names the occasion, which
        /// is the difference between a URL to fix and a provider to wait for. It
        /// never carries the request or the body, so nothing an operator did not
        /// write can reach the log through it.
        detail: Option<String>,
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
    /// A call that may not run until a person says so.
    ApprovalRequested {
        call: CallId,
        /// The instant this request stops being answerable.
        expires: Timestamp,
        risk: Risk,
        tool: String,
    },
    /// A person's answer, or the answer the expiry gives.
    ApprovalDecided {
        allow: bool,
        call: CallId,
        /// Why, when the answer was not a plain yes or no.
        note: Option<String>,
    },
    ToolIntent {
        args: String,
        call: CallId,
        /// The environment variable names the call had; never their values.
        env: Vec<String>,
        key: String,
        tool: String,
    },
    ToolResult {
        call: CallId,
        duration_ms: u64,
        /// What the tool wrote, cut to the manifest's bound when it was longer.
        output: String,
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
            Self::ApprovalRequested { .. } => taxonomy::APPROVAL_REQUESTED,
            Self::ApprovalDecided { .. } => taxonomy::APPROVAL_DECIDED,
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

/// One tool the model asked for.
///
/// The arguments are the provider's bytes, unparsed: this layer records what was
/// said, and reading JSON is the store's job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolRequest {
    pub args: String,
    pub tool: String,
}

#[cfg(test)]
pub(crate) mod tests {
    use super::{ErrorClass, EventKind, FinishReason, Risk, Role};
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
    fn every_risk_names_itself_and_answers_the_two_questions() {
        assert_eq!(Risk::Read.as_str(), "read");
        assert_eq!(Risk::Write.as_str(), "write");
        assert_eq!(Risk::Network.as_str(), "network");
        assert_eq!(Risk::Destructive.as_str(), "destructive");

        assert!(!Risk::Read.needs_approval());
        assert!(!Risk::Write.needs_approval());
        assert!(Risk::Network.needs_approval());
        assert!(Risk::Destructive.needs_approval());

        assert!(!Risk::Read.is_remembered());
        assert!(Risk::Network.is_remembered(), "one answer per branch");
        assert!(!Risk::Destructive.is_remembered(), "every time, by design");
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

    #[allow(
        clippy::too_many_lines,
        reason = "the point of this function is to list every kind in one place"
    )]
    pub fn sample_kinds() -> alloc::vec::Vec<EventKind> {
        use crate::ids::{BranchId, CallId, PrefixHash, TurnId};
        use alloc::string::{String, ToString};

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
                calls: alloc::vec::Vec::new(),
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
                detail: None,
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
            EventKind::ApprovalRequested {
                call,
                expires: crate::time::Timestamp::new(1_790_000_000, 0).expect("valid"),
                risk: Risk::Destructive,
                tool: "shell".to_string(),
            },
            EventKind::ApprovalDecided {
                allow: true,
                call,
                note: None,
            },
            EventKind::ToolIntent {
                args: "{}".to_string(),
                call,
                key: "k".to_string(),
                tool: "shell".to_string(),
                env: alloc::vec::Vec::new(),
            },
            EventKind::ToolResult {
                output: String::new(),
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
