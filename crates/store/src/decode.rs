//! Decoding. Turning the canonical bytes back into a typed event.
//!
//! This is the only place in the system that parses an event, and it lives in
//! the store rather than the core for one reason: the core writes and never
//! reads, so there is nothing in the core that can disagree with the writer
//! about what an event looks like.
//!
//! The payload of a type this build does not understand is kept as the exact
//! bytes it arrived as. `RawValue` borrows the original slice, so an unknown
//! event survives a decode and an encode unchanged, which is what makes the
//! round trip total rather than nearly total.

use std::fmt;
use std::str::FromStr;

use agent_core::event::{ErrorClass, Event, EventKind, FinishReason, Role};
use agent_core::ids::{Actor, BranchId, CallId, EventId, PrefixHash, TurnId};
use agent_core::taxonomy;
use agent_core::time::Timestamp;
use serde::Deserialize;

/// Why a stored body could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// The body is not JSON.
    Json(String),
    /// The body is JSON but not an object.
    NotAnObject,
    /// A required envelope attribute is missing.
    Missing(&'static str),
    /// An identifier or a timestamp in the envelope is malformed.
    Malformed(&'static str),
    /// The payload does not match its declared type.
    Payload(String),
}

impl fmt::Display for DecodeError {
    fn fmt(
        &self,
        formatter: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        match self {
            Self::Json(detail) => write!(formatter, "not JSON: {detail}"),
            Self::NotAnObject => formatter.write_str("the body is not a JSON object"),
            Self::Missing(attribute) => write!(formatter, "attribute {attribute} is missing"),
            Self::Malformed(attribute) => write!(formatter, "attribute {attribute} is malformed"),
            Self::Payload(detail) => write!(formatter, "payload does not match its type: {detail}"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// The envelope, with the payload left as the bytes it arrived as.
#[derive(Deserialize)]
struct Record<'a> {
    actor: &'a str,
    branch: &'a str,
    depth: u64,
    #[serde(default)]
    prev: Option<&'a str>,
    #[serde(default)]
    causationid: Option<&'a str>,
    #[serde(default)]
    correlationid: Option<&'a str>,
    #[serde(default)]
    model: Option<&'a str>,
    schemaver: u32,
    time: &'a str,
    #[serde(rename = "type")]
    type_name: &'a str,
    #[serde(borrow)]
    data: &'a serde_json::value::RawValue,
}

/// Reads one canonical body.
///
/// # Errors
///
/// Returns [`DecodeError::Json`] when the body is not JSON,
/// [`DecodeError::Malformed`] when an identifier or a timestamp does not parse,
/// and [`DecodeError::Payload`] when the payload does not match its type.
pub fn decode(body: &str) -> Result<Event, DecodeError> {
    let record: Record<'_> =
        serde_json::from_str(body).map_err(|error| DecodeError::Json(error.to_string()))?;

    let branch = BranchId::from_str(record.branch).map_err(|_| DecodeError::Malformed("branch"))?;
    let actor = Actor::from_str(record.actor).map_err(|_| DecodeError::Malformed("actor"))?;
    let time = Timestamp::parse(record.time).map_err(|_| DecodeError::Malformed("time"))?;
    let prev = optional_id::<EventId>(record.prev, "prev")?;
    let causation = optional_id::<EventId>(record.causationid, "causationid")?;
    let correlation = optional_id::<TurnId>(record.correlationid, "correlationid")?;

    Ok(Event {
        branch,
        depth: record.depth,
        prev,
        time,
        actor,
        causation,
        correlation,
        model: record.model.map(str::to_owned),
        kind: decode_kind(record.type_name, record.schemaver, record.data.get())?,
    })
}

fn optional_id<T: FromStr>(
    text: Option<&str>,
    attribute: &'static str,
) -> Result<Option<T>, DecodeError> {
    text.map_or(Ok(None), |value| {
        T::from_str(value)
            .map(Some)
            .map_err(|_| DecodeError::Malformed(attribute))
    })
}

#[derive(Deserialize)]
struct InstanceStarted {
    schema: u32,
    version: String,
}

#[derive(Deserialize)]
struct InstanceStopping {
    reason: String,
}

#[derive(Deserialize)]
struct InstanceRecovered {
    rolled_back: bool,
}

#[derive(Deserialize)]
struct InstanceClockSkew {
    delta_secs: i64,
}

#[derive(Deserialize)]
struct BranchCreated {
    #[serde(default)]
    parent: Option<String>,
    #[serde(default)]
    fork: Option<String>,
    #[serde(default)]
    label: Option<String>,
}

#[derive(Deserialize)]
struct BranchLabeled {
    label: String,
}

#[derive(Deserialize)]
struct BranchArchived {
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Deserialize)]
struct CommandReceived {
    command: String,
    command_id: String,
    surface: String,
    #[serde(default)]
    text: Option<String>,
}

#[derive(Deserialize)]
struct TurnRequested {
    input: String,
    turn: String,
}

#[derive(Deserialize)]
struct ModelRequested {
    prefix: String,
    segments: u32,
    tokens: u64,
    turn: String,
    warm: bool,
}

#[derive(Deserialize)]
struct ModelResponded {
    /// Absent from events written before the response could ask for a tool.
    #[serde(default)]
    calls: Vec<WireRequest>,
    finish: String,
    input_tokens: u64,
    output: String,
    output_tokens: u64,
    request: String,
    turn: String,
}

#[derive(Deserialize)]
struct WireRequest {
    args: String,
    tool: String,
}

#[derive(Deserialize)]
struct ModelCacheObserved {
    cached: u64,
    prefix: String,
    reported: bool,
    turn: String,
    uncached: u64,
}

#[derive(Deserialize)]
struct ModelFailed {
    attempt: u32,
    class: String,
    retryable: bool,
    turn: String,
}

#[derive(Deserialize)]
struct ScheduleArmed {
    attempt: u32,
    due: String,
    turn: String,
}

#[derive(Deserialize)]
struct MessageAppended {
    content: String,
    role: String,
    tokens: u64,
}

#[derive(Deserialize)]
struct ToolIntent {
    args: String,
    call: String,
    key: String,
    tool: String,
}

#[derive(Deserialize)]
struct ToolResult {
    call: String,
    duration_ms: u64,
    /// Absent from events written before a result carried what it produced.
    #[serde(default)]
    output: String,
    status: i64,
    truncated: bool,
}

#[derive(Deserialize)]
struct ToolFailed {
    call: String,
    class: String,
    may_have_happened: bool,
}

fn payload<'a, T: Deserialize<'a>>(raw: &'a str) -> Result<T, DecodeError> {
    serde_json::from_str(raw).map_err(|error| DecodeError::Payload(error.to_string()))
}

#[allow(
    clippy::too_many_lines,
    reason = "one flat table keeps the type strings and their payloads visible in one place; splitting it would scatter one format across six functions"
)]
fn decode_kind(
    type_name: &str,
    schema: u32,
    raw: &str,
) -> Result<EventKind, DecodeError> {
    if schema > taxonomy::SCHEMA_VERSION {
        return Ok(EventKind::Unknown {
            schema,
            data: raw.to_owned(),
        });
    }

    let kind = match type_name {
        taxonomy::INSTANCE_STARTED => {
            let payload: InstanceStarted = payload(raw)?;
            EventKind::InstanceStarted {
                schema: payload.schema,
                version: payload.version,
            }
        },
        taxonomy::INSTANCE_STOPPING => EventKind::InstanceStopping {
            reason: payload::<InstanceStopping>(raw)?.reason,
        },
        taxonomy::INSTANCE_RECOVERED => EventKind::InstanceRecovered {
            rolled_back: payload::<InstanceRecovered>(raw)?.rolled_back,
        },
        taxonomy::INSTANCE_CLOCK_SKEW => EventKind::InstanceClockSkew {
            delta_secs: payload::<InstanceClockSkew>(raw)?.delta_secs,
        },
        taxonomy::BRANCH_CREATED => {
            let payload: BranchCreated = payload(raw)?;
            EventKind::BranchCreated {
                parent: optional_id(payload.parent.as_deref(), "parent")
                    .map_err(|_| DecodeError::Payload("parent is not a branch id".to_owned()))?,
                fork: optional_id(payload.fork.as_deref(), "fork")
                    .map_err(|_| DecodeError::Payload("fork is not an event id".to_owned()))?,
                label: payload.label,
            }
        },
        taxonomy::BRANCH_LABELED => EventKind::BranchLabeled {
            label: payload::<BranchLabeled>(raw)?.label,
        },
        taxonomy::BRANCH_ARCHIVED => EventKind::BranchArchived {
            reason: payload::<BranchArchived>(raw)?.reason,
        },
        taxonomy::COMMAND_RECEIVED => {
            let payload: CommandReceived = payload(raw)?;
            EventKind::CommandReceived {
                command: payload.command,
                command_id: payload.command_id,
                surface: payload.surface,
                text: payload.text,
            }
        },
        taxonomy::TURN_REQUESTED => {
            let payload: TurnRequested = payload(raw)?;
            EventKind::TurnRequested {
                input: payload.input,
                turn: parse_turn(&payload.turn)?,
            }
        },
        taxonomy::MODEL_REQUESTED => {
            let payload: ModelRequested = payload(raw)?;
            EventKind::ModelRequested {
                prefix: parse_prefix(&payload.prefix)?,
                segments: payload.segments,
                tokens: payload.tokens,
                turn: parse_turn(&payload.turn)?,
                warm: payload.warm,
            }
        },
        taxonomy::MODEL_RESPONDED => {
            let payload: ModelResponded = payload(raw)?;
            EventKind::ModelResponded {
                calls: payload
                    .calls
                    .into_iter()
                    .map(|call| agent_core::event::ToolRequest {
                        args: call.args,
                        tool: call.tool,
                    })
                    .collect(),
                finish: parse_finish(&payload.finish)?,
                input_tokens: payload.input_tokens,
                output: payload.output,
                output_tokens: payload.output_tokens,
                request: payload.request,
                turn: parse_turn(&payload.turn)?,
            }
        },
        taxonomy::MODEL_CACHE_OBSERVED => {
            let payload: ModelCacheObserved = payload(raw)?;
            EventKind::ModelCacheObserved {
                cached: payload.cached,
                prefix: parse_prefix(&payload.prefix)?,
                reported: payload.reported,
                turn: parse_turn(&payload.turn)?,
                uncached: payload.uncached,
            }
        },
        taxonomy::MODEL_FAILED => {
            let payload: ModelFailed = payload(raw)?;
            EventKind::ModelFailed {
                attempt: payload.attempt,
                class: parse_error_class(&payload.class)?,
                retryable: payload.retryable,
                turn: parse_turn(&payload.turn)?,
            }
        },
        taxonomy::SCHEDULE_ARMED => {
            let payload: ScheduleArmed = payload(raw)?;
            EventKind::ScheduleArmed {
                attempt: payload.attempt,
                due: Timestamp::parse(&payload.due).map_err(|_| DecodeError::Malformed("due"))?,
                turn: parse_turn(&payload.turn)?,
            }
        },
        taxonomy::MESSAGE_APPENDED => {
            let payload: MessageAppended = payload(raw)?;
            EventKind::MessageAppended {
                content: payload.content,
                role: parse_role(&payload.role)?,
                tokens: payload.tokens,
            }
        },
        taxonomy::TOOL_INTENT => {
            let payload: ToolIntent = payload(raw)?;
            EventKind::ToolIntent {
                args: payload.args,
                call: parse_call(&payload.call)?,
                key: payload.key,
                tool: payload.tool,
            }
        },
        taxonomy::TOOL_RESULT => {
            let payload: ToolResult = payload(raw)?;
            EventKind::ToolResult {
                call: parse_call(&payload.call)?,
                duration_ms: payload.duration_ms,
                output: payload.output,
                status: i32::try_from(payload.status)
                    .map_err(|_| DecodeError::Payload("status is out of range".to_owned()))?,
                truncated: payload.truncated,
            }
        },
        taxonomy::TOOL_FAILED => {
            let payload: ToolFailed = payload(raw)?;
            EventKind::ToolFailed {
                call: parse_call(&payload.call)?,
                class: parse_error_class(&payload.class)?,
                may_have_happened: payload.may_have_happened,
            }
        },
        _ => EventKind::Unknown {
            schema,
            data: raw.to_owned(),
        },
    };
    Ok(kind)
}

fn parse_turn(text: &str) -> Result<TurnId, DecodeError> {
    TurnId::from_str(text).map_err(|_| DecodeError::Payload("turn is not a turn id".to_owned()))
}

fn parse_prefix(text: &str) -> Result<PrefixHash, DecodeError> {
    PrefixHash::from_str(text)
        .map_err(|_| DecodeError::Payload("prefix is not a prefix hash".to_owned()))
}

fn parse_call(text: &str) -> Result<CallId, DecodeError> {
    let (turn, step) = text
        .split_once(':')
        .ok_or_else(|| DecodeError::Payload("call is not a call id".to_owned()))?;
    Ok(CallId {
        turn: parse_turn(turn)?,
        step: u32::from_str(step)
            .map_err(|_| DecodeError::Payload("call step is not a number".to_owned()))?,
    })
}

fn parse_role(text: &str) -> Result<Role, DecodeError> {
    let role = match text {
        "system" => Role::System,
        "user" => Role::User,
        "assistant" => Role::Assistant,
        "tool" => Role::Tool,
        other => return Err(DecodeError::Payload(format!("unknown role {other}"))),
    };
    Ok(role)
}

fn parse_finish(text: &str) -> Result<FinishReason, DecodeError> {
    let finish = match text {
        "stop" => FinishReason::Stop,
        "length" => FinishReason::Length,
        "tool_calls" => FinishReason::ToolCalls,
        "content_filter" => FinishReason::ContentFilter,
        "unknown" => FinishReason::Unknown,
        other => {
            return Err(DecodeError::Payload(format!(
                "unknown finish reason {other}"
            )));
        },
    };
    Ok(finish)
}

fn parse_error_class(text: &str) -> Result<ErrorClass, DecodeError> {
    let class = match text {
        "transport" => ErrorClass::Transport,
        "timeout" => ErrorClass::Timeout,
        "rate_limited" => ErrorClass::RateLimited,
        "server_error" => ErrorClass::ServerError,
        "invalid_request" => ErrorClass::InvalidRequest,
        "context_length" => ErrorClass::ContextLength,
        "content_filter" => ErrorClass::ContentFilter,
        "cancelled" => ErrorClass::Cancelled,
        "unknown" => ErrorClass::Unknown,
        other => return Err(DecodeError::Payload(format!("unknown error class {other}"))),
    };
    Ok(class)
}

#[cfg(test)]
mod tests;
