use agent_core::event::{ErrorClass, Event, EventKind, FinishReason, Role};
use agent_core::ids::{Actor, BranchId, CallId, EventId, PrefixHash, TurnId};
use agent_core::time::Timestamp;

use super::{DecodeError, decode};

fn event(kind: EventKind) -> Event {
    Event {
        branch: BranchId::from_bytes([1; 8]),
        depth: 7,
        prev: Some(EventId::from_bytes([2; 32])),
        time: Timestamp::parse("2026-10-25T12:57:16Z").expect("parses"),
        actor: Actor::Agent("test".to_string()),
        causation: Some(EventId::from_bytes([3; 32])),
        correlation: Some(TurnId::from_bytes([4; 8])),
        model: Some("deepseek".to_string()),
        kind,
    }
}

fn turn() -> TurnId {
    TurnId::from_bytes([4; 8])
}

fn kinds() -> Vec<EventKind> {
    let call = CallId {
        turn: turn(),
        step: 2,
    };
    vec![
        EventKind::InstanceStarted {
            schema: 1,
            version: "0.0.0".to_string(),
        },
        EventKind::InstanceStopping {
            reason: "signal".to_string(),
        },
        EventKind::InstanceRecovered { rolled_back: true },
        EventKind::InstanceClockSkew { delta_secs: -3 },
        EventKind::BranchCreated {
            parent: Some(BranchId::from_bytes([5; 8])),
            fork: Some(EventId::from_bytes([6; 32])),
            label: Some("retry".to_string()),
        },
        EventKind::BranchLabeled {
            label: "main".to_string(),
        },
        EventKind::BranchArchived { reason: None },
        EventKind::CommandReceived {
            command: "send".to_string(),
            command_id: "c-1".to_string(),
            surface: "tui".to_string(),
            text: Some("hello".to_string()),
        },
        EventKind::TurnRequested {
            input: "hello".to_string(),
            turn: turn(),
        },
        EventKind::ModelRequested {
            prefix: PrefixHash::from_bytes([7; 32]),
            segments: 3,
            tokens: 4_096,
            turn: turn(),
            warm: false,
        },
        EventKind::ModelResponded {
            calls: vec![agent_core::event::ToolRequest {
                args: r#"{"cmd":"ls"}"#.to_string(),
                tool: "shell".to_string(),
            }],
            finish: FinishReason::ToolCalls,
            input_tokens: 100,
            output: "hi".to_string(),
            output_tokens: 2,
            request: "req-1".to_string(),
            turn: turn(),
        },
        EventKind::ModelCacheObserved {
            cached: 90,
            prefix: PrefixHash::from_bytes([8; 32]),
            reported: true,
            turn: turn(),
            uncached: 10,
        },
        EventKind::ModelFailed {
            attempt: 2,
            class: ErrorClass::RateLimited,
            retryable: true,
            turn: turn(),
        },
        EventKind::ScheduleArmed {
            attempt: 2,
            due: Timestamp::new(1_790_000_058, 500_000_000).expect("valid"),
            turn: turn(),
        },
        EventKind::MessageAppended {
            content: "a \"quoted\" line\nwith escapes".to_string(),
            role: Role::Assistant,
            tokens: 9,
        },
        EventKind::ToolIntent {
            args: "{\"argv\":[\"ls\"]}".to_string(),
            call,
            key: "sha256:abc".to_string(),
            tool: "shell".to_string(),
        },
        EventKind::ToolResult {
            output: String::new(),
            call,
            duration_ms: 12,
            status: -9,
            truncated: false,
        },
        EventKind::ToolFailed {
            call,
            class: ErrorClass::ContextLength,
            may_have_happened: true,
        },
    ]
}

#[test]
fn every_kind_survives_a_round_trip() {
    for kind in kinds() {
        let original = event(kind);
        let body = agent_core::canonical::canonical(&original);
        let decoded = decode(&body).expect("decodes");
        assert_eq!(decoded, original, "{body}");
    }
}

#[test]
fn the_canonical_bytes_survive_a_round_trip() {
    for kind in kinds() {
        let original = event(kind);
        let body = agent_core::canonical::canonical(&original);
        let re_encoded = agent_core::canonical::canonical(&decode(&body).expect("decodes"));
        assert_eq!(re_encoded, body);
    }
}

#[test]
fn absent_optional_attributes_decode_to_none() {
    let mut bare = event(EventKind::BranchArchived { reason: None });
    bare.prev = None;
    bare.causation = None;
    bare.correlation = None;
    bare.model = None;
    let body = agent_core::canonical::canonical(&bare);
    let decoded = decode(&body).expect("decodes");
    assert_eq!(decoded.prev, None);
    assert_eq!(decoded.causation, None);
    assert_eq!(decoded.correlation, None);
    assert_eq!(decoded.model, None);
}

#[test]
fn an_unknown_type_keeps_its_payload_bytes() {
    let payload = r#"{"z":1,"a":[true,null],"nested":{"data":"x"}}"#;
    let original = event(EventKind::Unknown {
        schema: 1,
        data: payload.to_string(),
    });
    let body = agent_core::canonical::canonical(&original);
    let decoded = decode(&body).expect("decodes");
    assert_eq!(decoded, original);
    assert_eq!(agent_core::canonical::canonical(&decoded), body);
}

#[test]
fn a_newer_schema_becomes_unknown_rather_than_truncated() {
    let body = agent_core::canonical::canonical(&event(EventKind::InstanceStarted {
        schema: 1,
        version: "0.0.0".to_string(),
    }))
    .replace(r#""schemaver":1"#, r#""schemaver":2"#);
    let decoded = decode(&body).expect("decodes");
    match decoded.kind {
        EventKind::Unknown { schema, data } => {
            assert_eq!(schema, 2);
            assert!(data.contains(r#""version":"0.0.0""#), "{data}");
        },
        other => panic!("expected unknown, got {other:?}"),
    }
}

#[test]
fn a_body_that_is_not_json_is_refused() {
    assert!(matches!(decode("not json"), Err(DecodeError::Json(_))));
    assert!(matches!(decode("[1,2]"), Err(DecodeError::Json(_))));
}

#[test]
fn a_malformed_branch_is_refused() {
    let body = agent_core::canonical::canonical(&event(EventKind::InstanceRecovered {
        rolled_back: true,
    }))
    .replace(r#""branch":"b-0101010101010101""#, r#""branch":"nope""#);
    assert_eq!(decode(&body), Err(DecodeError::Malformed("branch")));
}

#[test]
fn a_malformed_actor_is_refused() {
    let body = agent_core::canonical::canonical(&event(EventKind::InstanceRecovered {
        rolled_back: true,
    }))
    .replace(r#""actor":"agent:test""#, r#""actor":"robot""#);
    assert_eq!(decode(&body), Err(DecodeError::Malformed("actor")));
}

#[test]
fn a_malformed_timestamp_is_refused() {
    let body = agent_core::canonical::canonical(&event(EventKind::InstanceRecovered {
        rolled_back: true,
    }))
    .replace(r#""time":"2026-10-25T12:57:16Z""#, r#""time":"yesterday""#);
    assert_eq!(decode(&body), Err(DecodeError::Malformed("time")));
}

#[test]
fn a_malformed_predecessor_is_refused() {
    let body = agent_core::canonical::canonical(&event(EventKind::InstanceRecovered {
        rolled_back: true,
    }))
    .replace(r#""prev":"0202"#, r#""prev":"zzzz"#);
    assert_eq!(decode(&body), Err(DecodeError::Malformed("prev")));
}

#[test]
fn a_malformed_correlation_is_refused() {
    let body = agent_core::canonical::canonical(&event(EventKind::InstanceRecovered {
        rolled_back: true,
    }))
    .replace(
        r#""correlationid":"0404040404040404""#,
        r#""correlationid":"nope""#,
    );
    assert_eq!(decode(&body), Err(DecodeError::Malformed("correlationid")));
}

#[test]
fn a_payload_that_does_not_match_its_type_is_refused() {
    let body = agent_core::canonical::canonical(&event(EventKind::TurnRequested {
        input: "hello".to_string(),
        turn: turn(),
    }))
    .replace(r#""turn":"0404040404040404""#, r#""turn":"nope""#);
    assert!(matches!(decode(&body), Err(DecodeError::Payload(_))));
}

#[test]
fn a_missing_payload_field_is_refused() {
    let body = agent_core::canonical::canonical(&event(EventKind::TurnRequested {
        input: "hello".to_string(),
        turn: turn(),
    }))
    .replace(r#""input":"hello","turn":"#, r#""turn":"#);
    assert!(matches!(decode(&body), Err(DecodeError::Payload(_))));
}

#[test]
fn an_unknown_role_is_refused() {
    let body = agent_core::canonical::canonical(&event(EventKind::MessageAppended {
        content: "x".to_string(),
        role: Role::User,
        tokens: 1,
    }))
    .replace(r#""role":"user""#, r#""role":"ghost""#);
    assert!(matches!(decode(&body), Err(DecodeError::Payload(_))));
}

#[test]
fn the_unknown_finish_reason_round_trips() {
    let original = event(EventKind::ModelResponded {
        output: String::new(),
        calls: Vec::new(),
        finish: FinishReason::Unknown,
        input_tokens: 1,
        output_tokens: 1,
        request: "r".to_string(),
        turn: turn(),
    });
    let body = agent_core::canonical::canonical(&original);
    assert_eq!(decode(&body).expect("decodes"), original);
}

#[test]
fn an_unknown_finish_reason_is_refused() {
    let body = agent_core::canonical::canonical(&event(EventKind::ModelResponded {
        output: String::new(),
        calls: Vec::new(),
        finish: FinishReason::Stop,
        input_tokens: 1,
        output_tokens: 1,
        request: "r".to_string(),
        turn: turn(),
    }))
    .replace(r#""finish":"stop""#, r#""finish":"pause""#);
    assert!(matches!(decode(&body), Err(DecodeError::Payload(_))));
}

#[test]
fn an_unknown_error_class_is_refused() {
    let body = agent_core::canonical::canonical(&event(EventKind::ModelFailed {
        attempt: 1,
        class: ErrorClass::Timeout,
        retryable: true,
        turn: turn(),
    }))
    .replace(r#""class":"timeout""#, r#""class":"gremlin""#);
    assert!(matches!(decode(&body), Err(DecodeError::Payload(_))));
}

#[test]
fn a_call_id_that_is_not_a_pair_is_refused() {
    let body = agent_core::canonical::canonical(&event(EventKind::ToolIntent {
        args: "{}".to_string(),
        call: CallId {
            turn: turn(),
            step: 0,
        },
        key: "k".to_string(),
        tool: "shell".to_string(),
    }))
    .replace(r#""call":"0404040404040404:0""#, r#""call":"nocolon""#);
    assert!(matches!(decode(&body), Err(DecodeError::Payload(_))));
}

#[test]
fn a_call_step_that_is_not_a_number_is_refused() {
    let body = agent_core::canonical::canonical(&event(EventKind::ToolIntent {
        args: "{}".to_string(),
        call: CallId {
            turn: turn(),
            step: 0,
        },
        key: "k".to_string(),
        tool: "shell".to_string(),
    }))
    .replace(r#":0""#, r#":x""#);
    assert!(matches!(decode(&body), Err(DecodeError::Payload(_))));
}

#[test]
fn a_status_out_of_range_is_refused() {
    let body = agent_core::canonical::canonical(&event(EventKind::ToolResult {
        call: CallId {
            turn: turn(),
            step: 0,
        },
        duration_ms: 0,
        output: String::new(),
        status: 0,
        truncated: false,
    }))
    .replace(r#""status":0"#, r#""status":99999999999"#);
    assert!(matches!(decode(&body), Err(DecodeError::Payload(_))));
}

#[test]
fn a_parent_that_is_not_a_branch_is_refused() {
    let body = agent_core::canonical::canonical(&event(EventKind::BranchCreated {
        parent: Some(BranchId::from_bytes([5; 8])),
        fork: None,
        label: None,
    }))
    .replace(r#""parent":"b-0505050505050505""#, r#""parent":"nope""#);
    assert!(matches!(decode(&body), Err(DecodeError::Payload(_))));
}

#[test]
fn every_error_explains_itself() {
    let errors = [
        DecodeError::Json("bad".to_string()),
        DecodeError::Malformed("branch"),
        DecodeError::Payload("detail".to_string()),
    ];
    for error in errors {
        assert_ne!(error.to_string(), "");
    }
}
