//! The canonical encoding. This is the only place in the system that decides
//! what an event's bytes are, and therefore the only place that decides its id.
//!
//! The shape follows the JSON Canonicalization Scheme: object keys in
//! lexicographic order, no insignificant whitespace, the two character escapes
//! the scheme requires for control characters, and every other character
//! written as itself. Numbers are integers only, which removes the float
//! formatting rules that make a general canonicalizer long.

use alloc::string::String;

use crate::event::{Event, EventKind, ToolRequest};
use crate::ids::EventId;

const SOURCE: &str = "agent://mokmok";
const SPECVERSION: &str = "1.0";
const DATACONTENTTYPE: &str = "application/json";

/// Writes one event as canonical JSON.
///
/// The envelope keys are emitted in sorted order by construction, so a reader
/// can compare two encodings byte for byte and see the smallest difference.
pub fn write_event(
    event: &Event,
    out: &mut String,
) {
    let mut object = Object::new(out);
    object.string("actor", &alloc::format!("{}", event.actor));
    object.string("branch", &alloc::format!("{}", event.branch));
    if let Some(causation) = &event.causation {
        object.string("causationid", &alloc::format!("{causation}"));
    }
    if let Some(correlation) = &event.correlation {
        object.string("correlationid", &alloc::format!("{correlation}"));
    }
    object.key("data");
    write_data(&event.kind, object.out);
    object.string("datacontenttype", DATACONTENTTYPE);
    object.uint("depth", event.depth);
    if let Some(model) = &event.model {
        object.string("model", model);
    }
    if let Some(prev) = &event.prev {
        object.string("prev", &alloc::format!("{prev}"));
    }
    object.uint("schemaver", u64::from(event.schema_version()));
    object.string("source", SOURCE);
    object.string("specversion", SPECVERSION);
    object.string("subject", &event.subject());
    object.string("time", &event.time.rfc3339());
    object.string("type", event.type_name());
    object.finish();
}

/// The canonical bytes of one event.
#[must_use]
pub fn canonical(event: &Event) -> String {
    let mut out = String::new();
    write_event(event, &mut out);
    out
}

/// The event's identity: the BLAKE3 hash of its canonical bytes.
#[must_use]
pub fn event_id(event: &Event) -> EventId {
    EventId::from_bytes(*crate::hash::digest(canonical(event).as_bytes()).as_bytes())
}

struct Object<'a> {
    out: &'a mut String,
    first: bool,
}

impl<'a> Object<'a> {
    fn new(out: &'a mut String) -> Self {
        out.push('{');
        Self { out, first: true }
    }

    fn key(
        &mut self,
        key: &str,
    ) {
        if self.first {
            self.first = false;
        } else {
            self.out.push(',');
        }
        write_string(key, self.out);
        self.out.push(':');
    }

    fn string(
        &mut self,
        key: &str,
        value: &str,
    ) {
        self.key(key);
        write_string(value, self.out);
    }

    fn uint(
        &mut self,
        key: &str,
        value: u64,
    ) {
        self.key(key);
        write_uint(value, self.out);
    }

    fn int(
        &mut self,
        key: &str,
        value: i64,
    ) {
        self.key(key);
        write_int(value, self.out);
    }

    fn boolean(
        &mut self,
        key: &str,
        value: bool,
    ) {
        self.key(key);
        self.out.push_str(if value { "true" } else { "false" });
    }

    /// A list of objects, keys in the order the payload declares them.
    fn requests(
        &mut self,
        key: &str,
        requests: &[ToolRequest],
    ) {
        self.key(key);
        self.out.push('[');
        for (index, request) in requests.iter().enumerate() {
            if index > 0 {
                self.out.push(',');
            }
            self.out.push_str("{\"args\":");
            write_string(&request.args, self.out);
            self.out.push_str(",\"tool\":");
            write_string(&request.tool, self.out);
            self.out.push('}');
        }
        self.out.push(']');
    }

    fn finish(&mut self) {
        self.out.push('}');
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "one flat table keeps the key order of every payload visible in one place; six helpers would each hold a fragment of one format"
)]
fn write_data(
    kind: &EventKind,
    out: &mut String,
) {
    if let EventKind::Unknown { data, .. } = kind {
        out.push_str(data);
        return;
    }
    let mut object = Object::new(out);
    match kind {
        EventKind::InstanceStarted { schema, version } => {
            object.uint("schema", u64::from(*schema));
            object.string("version", version);
        },
        EventKind::InstanceStopping { reason } => object.string("reason", reason),
        EventKind::InstanceRecovered { rolled_back } => object.boolean("rolled_back", *rolled_back),
        EventKind::InstanceClockSkew { delta_secs } => object.int("delta_secs", *delta_secs),
        EventKind::BranchCreated {
            parent,
            fork,
            label,
        } => {
            if let Some(fork) = fork {
                object.string("fork", &alloc::format!("{fork}"));
            }
            if let Some(label) = label {
                object.string("label", label);
            }
            if let Some(parent) = parent {
                object.string("parent", &alloc::format!("{parent}"));
            }
        },
        EventKind::BranchLabeled { label } => object.string("label", label),
        EventKind::BranchArchived { reason } => {
            if let Some(reason) = reason {
                object.string("reason", reason);
            }
        },
        EventKind::CommandReceived {
            command,
            command_id,
            surface,
            text,
        } => {
            object.string("command", command);
            object.string("command_id", command_id);
            object.string("surface", surface);
            if let Some(text) = text {
                object.string("text", text);
            }
        },
        EventKind::TurnRequested { input, turn } => {
            object.string("input", input);
            object.string("turn", &alloc::format!("{turn}"));
        },
        EventKind::ModelRequested {
            prefix,
            segments,
            tokens,
            turn,
            warm,
        } => {
            object.string("prefix", &alloc::format!("{prefix}"));
            object.uint("segments", u64::from(*segments));
            object.uint("tokens", *tokens);
            object.string("turn", &alloc::format!("{turn}"));
            object.boolean("warm", *warm);
        },
        EventKind::ModelResponded {
            calls,
            finish,
            input_tokens,
            output,
            output_tokens,
            request,
            turn,
        } => {
            object.requests("calls", calls);
            object.string("finish", finish.as_str());
            object.uint("input_tokens", *input_tokens);
            object.string("output", output);
            object.uint("output_tokens", *output_tokens);
            object.string("request", request);
            object.string("turn", &alloc::format!("{turn}"));
        },
        EventKind::ModelCacheObserved {
            cached,
            prefix,
            reported,
            turn,
            uncached,
        } => {
            object.uint("cached", *cached);
            object.string("prefix", &alloc::format!("{prefix}"));
            object.boolean("reported", *reported);
            object.string("turn", &alloc::format!("{turn}"));
            object.uint("uncached", *uncached);
        },
        EventKind::ModelFailed {
            attempt,
            class,
            retryable,
            turn,
        } => {
            object.uint("attempt", u64::from(*attempt));
            object.string("class", class.as_str());
            object.boolean("retryable", *retryable);
            object.string("turn", &alloc::format!("{turn}"));
        },
        EventKind::ScheduleArmed { attempt, due, turn } => {
            object.uint("attempt", u64::from(*attempt));
            object.string("due", &due.rfc3339());
            object.string("turn", &alloc::format!("{turn}"));
        },
        EventKind::MessageAppended {
            content,
            role,
            tokens,
        } => {
            object.string("content", content);
            object.string("role", role.as_str());
            object.uint("tokens", *tokens);
        },
        EventKind::ToolIntent {
            args,
            call,
            key,
            tool,
        } => {
            object.string("args", args);
            object.string("call", &alloc::format!("{call}"));
            object.string("key", key);
            object.string("tool", tool);
        },
        EventKind::ToolResult {
            call,
            duration_ms,
            status,
            truncated,
        } => {
            object.string("call", &alloc::format!("{call}"));
            object.uint("duration_ms", *duration_ms);
            object.int("status", i64::from(*status));
            object.boolean("truncated", *truncated);
        },
        EventKind::ToolFailed {
            call,
            class,
            may_have_happened,
        } => {
            object.string("call", &alloc::format!("{call}"));
            object.string("class", class.as_str());
            object.boolean("may_have_happened", *may_have_happened);
        },
        EventKind::Unknown { .. } => {},
    }
    object.finish();
}

fn write_string(
    value: &str,
    out: &mut String,
) {
    out.push('"');
    for character in value.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            control if u32::from(control) < 0x20 => {
                out.push_str("\\u00");
                let value = u32::from(control);
                out.push(hex_digit(u8::try_from(value >> 4).unwrap_or(0)));
                out.push(hex_digit(u8::try_from(value & 0x0f).unwrap_or(0)));
            },
            other => out.push(other),
        }
    }
    out.push('"');
}

fn hex_digit(value: u8) -> char {
    char::from(b"0123456789abcdef"[usize::from(value)])
}

fn write_uint(
    mut value: u64,
    out: &mut String,
) {
    let mut buffer = [0_u8; 20];
    let mut index = buffer.len();
    loop {
        index -= 1;
        buffer[index] = b'0' + u8::try_from(value % 10).unwrap_or(0);
        value /= 10;
        if value == 0 {
            break;
        }
    }
    out.push_str(core::str::from_utf8(&buffer[index..]).unwrap_or_default());
}

fn write_int(
    value: i64,
    out: &mut String,
) {
    if value < 0 {
        out.push('-');
        write_uint(value.unsigned_abs(), out);
    } else {
        write_uint(value.unsigned_abs(), out);
    }
}

#[cfg(test)]
mod tests {
    use super::{canonical, event_id};
    use crate::event::{ErrorClass, Event, EventKind, FinishReason, Role, ToolRequest};
    use crate::ids::{Actor, BranchId, CallId, EventId, PrefixHash, TurnId};
    use crate::time::Timestamp;
    use alloc::string::{String, ToString};

    const BRANCH: [u8; 8] = [0x01; 8];
    const TURN: [u8; 8] = [0x02; 8];

    fn event(kind: EventKind) -> Event {
        Event {
            branch: BranchId::from_bytes(BRANCH),
            depth: 0,
            prev: None,
            time: Timestamp::parse("2026-10-25T12:57:16Z").expect("parses"),
            actor: Actor::Human("ana".to_string()),
            causation: None,
            correlation: None,
            model: None,
            kind,
        }
    }

    fn turn() -> TurnId {
        TurnId::from_bytes(TURN)
    }

    #[test]
    fn a_minimal_event_encodes_without_optional_keys() {
        let encoded = canonical(&event(EventKind::InstanceStarted {
            schema: 1,
            version: "0.0.0".to_string(),
        }));
        assert_eq!(
            encoded,
            r#"{"actor":"human:ana","branch":"b-0101010101010101","data":{"schema":1,"version":"0.0.0"},"datacontenttype":"application/json","depth":0,"schemaver":1,"source":"agent://mokmok","specversion":"1.0","subject":"branch/b-0101010101010101","time":"2026-10-25T12:57:16Z","type":"dev.mokmok.agent.instance.started"}"#
        );
    }

    #[test]
    fn a_full_event_encodes_every_optional_key_in_order() {
        let mut full = event(EventKind::InstanceStopping {
            reason: "signal".to_string(),
        });
        full.depth = 41;
        full.prev = Some(EventId::from_bytes([0x09; 32]));
        full.causation = Some(EventId::from_bytes([0x0a; 32]));
        full.correlation = Some(turn());
        full.model = Some("deepseek-v4.1-flash".to_string());
        assert_eq!(
            canonical(&full),
            r#"{"actor":"human:ana","branch":"b-0101010101010101","causationid":"0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a","correlationid":"0202020202020202","data":{"reason":"signal"},"datacontenttype":"application/json","depth":41,"model":"deepseek-v4.1-flash","prev":"0909090909090909090909090909090909090909090909090909090909090909","schemaver":1,"source":"agent://mokmok","specversion":"1.0","subject":"branch/b-0101010101010101","time":"2026-10-25T12:57:16Z","type":"dev.mokmok.agent.instance.stopping"}"#
        );
    }

    #[test]
    fn the_id_is_the_hash_of_the_canonical_bytes() {
        let instance = event(EventKind::InstanceRecovered { rolled_back: false });
        assert_eq!(
            instance.id(),
            EventId::from_bytes(*blake3::hash(canonical(&instance).as_bytes()).as_bytes())
        );
        assert_eq!(event_id(&instance), instance.id());
    }

    #[test]
    fn the_id_changes_when_any_field_changes() {
        let base = event(EventKind::InstanceRecovered { rolled_back: false });
        let mut changed = base.clone();
        changed.depth = 1;
        assert_ne!(base.id(), changed.id());
    }

    #[test]
    fn the_same_event_encodes_the_same_twice() {
        let base = event(EventKind::InstanceRecovered { rolled_back: true });
        assert_eq!(canonical(&base), canonical(&base.clone()));
    }

    #[test]
    fn a_string_is_escaped_the_way_the_scheme_requires() {
        let encoded = canonical(&event(EventKind::MessageAppended {
            content: "a\"b\\c\nd\re\tf\u{08}g\u{0c}h\u{01}i\u{1f}j é".to_string(),
            role: Role::User,
            tokens: 0,
        }));
        assert!(
            encoded.contains(r#""content":"a\"b\\c\nd\re\tf\bg\fh\u0001i\u001fj é""#),
            "{encoded}"
        );
    }

    #[test]
    fn every_kind_encodes() {
        for kind in super::super::event::tests::sample_kinds() {
            let encoded = canonical(&event(kind));
            assert!(encoded.starts_with('{'));
            assert!(encoded.ends_with('}'));
        }
    }

    #[test]
    fn a_negative_number_keeps_its_sign() {
        let encoded = canonical(&event(EventKind::InstanceClockSkew { delta_secs: -1_500 }));
        assert!(
            encoded.contains(r#""data":{"delta_secs":-1500}"#),
            "{encoded}"
        );
    }

    #[test]
    fn a_large_number_is_written_in_full() {
        let encoded = canonical(&event(EventKind::MessageAppended {
            content: String::new(),
            role: Role::System,
            tokens: u64::MAX,
        }));
        assert!(
            encoded.contains(r#""tokens":18446744073709551615"#),
            "{encoded}"
        );
    }

    #[test]
    fn a_zero_number_is_written_without_a_sign() {
        let encoded = canonical(&event(EventKind::ToolResult {
            call: CallId {
                turn: turn(),
                step: 0,
            },
            duration_ms: 0,
            status: 0,
            truncated: false,
        }));
        assert!(encoded.contains(r#""status":0"#), "{encoded}");
        assert!(encoded.contains(r#""duration_ms":0"#), "{encoded}");
    }

    #[test]
    fn a_negative_tool_status_keeps_its_sign() {
        let call = CallId {
            turn: turn(),
            step: 3,
        };
        let encoded = canonical(&event(EventKind::ToolResult {
            call,
            duration_ms: 0,
            status: -9,
            truncated: true,
        }));
        assert!(encoded.contains("\"status\":-9"), "{encoded}");
        assert!(encoded.contains("\"truncated\":true"), "{encoded}");
    }

    #[test]
    fn an_unknown_payload_is_copied_through_verbatim() {
        let encoded = canonical(&event(EventKind::Unknown {
            schema: 4,
            data: r#"{"b":2,"a":[1,true]}"#.to_string(),
        }));
        assert!(
            encoded.contains(r#""data":{"b":2,"a":[1,true]}"#),
            "{encoded}"
        );
        assert!(encoded.contains(r#""schemaver":4"#), "{encoded}");
        assert!(encoded.contains(r#""type":"""#), "{encoded}");
    }

    #[test]
    fn a_model_event_carries_its_identifiers() {
        let encoded = canonical(&event(EventKind::ModelRequested {
            prefix: PrefixHash::from_bytes([0x11; 32]),
            segments: 3,
            tokens: 2_048,
            turn: turn(),
            warm: true,
        }));
        assert!(
            encoded.contains(
                r#""prefix":"1111111111111111111111111111111111111111111111111111111111111111""#
            ),
            "{encoded}"
        );
        assert!(
            encoded.contains(r#""turn":"0202020202020202""#),
            "{encoded}"
        );
        assert!(encoded.contains(r#""warm":true"#), "{encoded}");
    }

    #[test]
    fn a_failure_carries_its_class() {
        let encoded = canonical(&event(EventKind::ToolFailed {
            call: CallId {
                turn: turn(),
                step: 0,
            },
            class: ErrorClass::ContextLength,
            may_have_happened: false,
        }));
        assert!(encoded.contains(r#""class":"context_length""#), "{encoded}");
        assert!(
            encoded.contains(r#""may_have_happened":false"#),
            "{encoded}"
        );
    }

    #[test]
    fn a_response_carries_its_finish_reason() {
        let encoded = canonical(&event(EventKind::ModelResponded {
            calls: alloc::vec::Vec::new(),
            finish: FinishReason::ToolCalls,
            input_tokens: 1,
            output: "x".to_string(),
            output_tokens: 1,
            request: "r".to_string(),
            turn: turn(),
        }));
        assert!(encoded.contains(r#""finish":"tool_calls""#), "{encoded}");
    }

    #[test]
    fn a_response_that_asked_for_tools_names_them() {
        let encoded = canonical(&event(EventKind::ModelResponded {
            calls: alloc::vec![
                ToolRequest {
                    args: r#"{"cmd":"ls"}"#.to_string(),
                    tool: "shell".to_string(),
                },
                ToolRequest {
                    args: "{}".to_string(),
                    tool: "clock".to_string(),
                },
            ],
            finish: FinishReason::ToolCalls,
            input_tokens: 1,
            output: String::new(),
            output_tokens: 1,
            request: "r".to_string(),
            turn: turn(),
        }));
        assert!(
            encoded.contains(r#""calls":[{"args":"{\"cmd\":\"ls\"}","tool":"shell"},{"args":"{}","tool":"clock"}]"#),
            "{encoded}"
        );
    }

    #[test]
    fn a_branch_creation_can_name_its_parent() {
        let encoded = canonical(&event(EventKind::BranchCreated {
            parent: Some(BranchId::from_bytes([0x07; 8])),
            fork: Some(EventId::from_bytes([0x08; 32])),
            label: Some("retry".to_string()),
        }));
        assert!(encoded.contains(r#""data":{"fork":"0808080808080808080808080808080808080808080808080808080808080808","label":"retry","parent":"b-0707070707070707"}"#), "{encoded}");
    }
}
