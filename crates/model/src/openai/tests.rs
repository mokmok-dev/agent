use agent_core::event::{FinishReason, Role};

use super::{Calls, Chunk, CodecError, DONE, Usage, decode, encode, finish_of};
use crate::{Message, Request};
use agent_prompt::CallRef;

fn request() -> Request {
    Request {
        model: "deepseek".to_string(),
        directive: "You are a careful agent.".to_string(),
        tools: Some(
            r#"[{"type":"function","function":{"name":"read","description":"read a file","parameters":{"type":"object","properties":{"path":{"type":"string"}}}}}]"#
                .to_string(),
        ),
        messages: vec![
            Message {
                call: None,
                calls: Vec::new(),
                role: Role::User,
                content: "hello".to_string(),
            },
            Message {
                call: None,
                calls: Vec::new(),
                role: Role::Assistant,
                content: "hi".to_string(),
            },
        ],
        max_output_tokens: 512,
        input_tokens_estimate: 40,
    }
}

#[test]
fn the_body_carries_the_model_the_cap_and_streaming() {
    let body = encode(&request());
    assert!(body.contains(r#""model":"deepseek""#), "{body}");
    assert!(body.contains(r#""stream":true"#), "{body}");
    assert!(body.contains(r#""include_usage":true"#), "{body}");
    assert!(body.contains(r#""max_tokens":512"#), "{body}");
}

#[test]
fn the_directive_is_the_only_system_message_and_the_tools_are_a_field() {
    let body = encode(&request());
    assert_eq!(
        body.matches(r#""role":"system""#).count(),
        1,
        "the tool block is not a second system message: {body}"
    );
    assert!(body.contains(r#""tools":["#), "{body}");
    assert!(body.contains(r#""tool_choice":"auto""#), "{body}");
    assert!(body.contains(r#""name":"read""#), "{body}");
}

#[test]
fn the_messages_keep_their_order_and_their_roles() {
    let body = encode(&request());
    let hello = body.find("hello").expect("the first message");
    let hi = body.find("hi").expect("the second message");
    assert!(hello < hi, "{body}");
    assert!(body.contains(r#""role":"assistant""#), "{body}");
}

#[test]
fn an_empty_tool_block_is_omitted_rather_than_sent_blank() {
    let mut bare = request();
    bare.tools = None;
    let body = encode(&bare);
    assert_eq!(body.matches(r#""role":"system""#).count(), 1, "{body}");
    assert!(!body.contains(r#""tools":"#), "{body}");
}

#[test]
fn a_quote_in_a_message_is_escaped_rather_than_breaking_the_body() {
    let mut quoted = request();
    quoted.messages[0].content = "he said \"hi\"\n".to_string();
    let body = encode(&quoted);
    assert!(body.contains(r#"he said \"hi\"\n"#), "{body}");
    let parsed: serde_json::Value = serde_json::from_str(&body).expect("valid json");
    assert_eq!(
        parsed["messages"][1]["content"].as_str(),
        Some("he said \"hi\"\n")
    );
}

#[test]
fn a_text_delta_is_the_text() {
    let frame = r#"{"choices":[{"index":0,"delta":{"content":"he"},"finish_reason":null}]}"#;
    let chunk = decode(frame).expect("decodes");
    assert_eq!(chunk.text.as_deref(), Some("he"));
    assert_eq!(chunk.finish, None);
    assert_eq!(chunk.usage, None);
    assert!(!chunk.done);
}

#[test]
fn a_finish_reason_is_carried_untranslated() {
    let frame = r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#;
    let chunk = decode(frame).expect("decodes");
    assert_eq!(chunk.finish.as_deref(), Some("tool_calls"));
    assert_eq!(chunk.text, None);
}

#[test]
fn a_usage_frame_carries_the_counts_and_the_cached_share() {
    let frame = r#"{"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":2,"prompt_tokens_details":{"cached_tokens":90}}}"#;
    let chunk = decode(frame).expect("decodes");
    assert_eq!(
        chunk.usage,
        Some(Usage {
            input: 100,
            output: 2,
            cached: Some(90),
        })
    );
}

#[test]
fn a_usage_frame_without_a_cached_count_says_nothing_about_the_cache() {
    let frame = r#"{"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":2}}"#;
    let chunk = decode(frame).expect("decodes");
    assert_eq!(
        chunk.usage,
        Some(Usage {
            input: 100,
            output: 2,
            cached: None,
        })
    );
}

#[test]
fn a_null_cached_count_is_not_a_zero() {
    let frame = r#"{"choices":[],"usage":{"prompt_tokens":5,"completion_tokens":1,"prompt_tokens_details":{"cached_tokens":null}}}"#;
    let chunk = decode(frame).expect("decodes");
    assert_eq!(chunk.usage.expect("usage").cached, None);
}

#[test]
fn the_done_sentinel_is_recognised() {
    let chunk = decode(DONE).expect("decodes");
    assert!(chunk.done);
    assert_eq!(chunk.text, None);
}

#[test]
fn a_frame_with_nothing_in_it_is_not_an_error() {
    let chunk = decode("{}").expect("decodes");
    assert_eq!(chunk, Chunk::default());
}

#[test]
fn a_field_this_build_does_not_know_is_ignored() {
    let frame = r#"{"id":"x","object":"chat.completion.chunk","choices":[{"delta":{"content":"a","reasoning":"r"}}],"system_fingerprint":"abc"}"#;
    let chunk = decode(frame).expect("decodes");
    assert_eq!(chunk.text.as_deref(), Some("a"));
}

#[test]
fn a_null_content_is_no_text_rather_than_an_empty_string() {
    let frame = r#"{"choices":[{"delta":{"content":null}}]}"#;
    let chunk = decode(frame).expect("decodes");
    assert_eq!(chunk.text, None);
}

#[test]
fn a_frame_that_is_not_json_is_refused() {
    let error = decode("not json").expect_err("refuses");
    match error {
        CodecError::Json(detail) => assert!(
            detail.contains("expected") || !detail.is_empty(),
            "{detail}"
        ),
    }
}

#[test]
fn an_error_explains_itself() {
    assert_ne!(CodecError::Json("detail".to_string()).to_string(), "");
}

#[test]
fn the_four_known_finish_reasons_map_exactly() {
    assert_eq!(finish_of("stop"), FinishReason::Stop);
    assert_eq!(finish_of("length"), FinishReason::Length);
    assert_eq!(finish_of("tool_calls"), FinishReason::ToolCalls);
    assert_eq!(finish_of("function_call"), FinishReason::ToolCalls);
    assert_eq!(finish_of("content_filter"), FinishReason::ContentFilter);
}

#[test]
fn a_finish_reason_this_build_does_not_know_is_kept_as_unknown() {
    assert_eq!(finish_of("eos"), FinishReason::Unknown);
    assert_eq!(finish_of(""), FinishReason::Unknown);
}

#[test]
fn a_tool_call_arrives_in_pieces_and_is_joined_back_together() {
    let first = decode(
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"shell","arguments":"{\"cmd\":"}}]}}]}"#,
    )
    .expect("reads");
    let second = decode(
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"ls\"}"}}]}}]}"#,
    )
    .expect("reads");
    let mut calls = Calls::default();
    calls.absorb(&first.calls);
    calls.absorb(&second.calls);
    let calls = calls.finish();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].id, "c1");
    assert_eq!(calls[0].name, "shell");
    assert_eq!(calls[0].arguments, r#"{"cmd":"ls"}"#);
}

#[test]
fn two_calls_are_kept_in_the_order_their_indices_give() {
    let frame = decode(
        r#"{"choices":[{"delta":{"tool_calls":[{"index":1,"id":"c2","function":{"name":"clock","arguments":"{}"}},{"index":0,"id":"c1","function":{"name":"shell","arguments":"{}"}}]}}]}"#,
    )
    .expect("reads");
    let mut calls = Calls::default();
    calls.absorb(&frame.calls);
    let finished = calls.finish();
    let names: Vec<&str> = finished.iter().map(|call| call.name.as_str()).collect();
    assert_eq!(names, vec!["shell", "clock"]);
}

#[test]
fn a_frame_without_tools_leaves_the_calls_alone() {
    let frame = decode(r#"{"choices":[{"delta":{"content":"hi"}}]}"#).expect("reads");
    assert_eq!(frame.calls.len(), 0);
}

#[test]
fn a_message_that_asked_for_tools_carries_them_on_the_wire() {
    let calls = vec![CallRef {
        args: r#"{"cmd":"date"}"#.to_string(),
        id: "t:0".to_string(),
        tool: "shell".to_string(),
    }];
    let prompt = crate::tests::prompt(&[agent_prompt::Message {
        call: None,
        calls: &calls,
        role: agent_core::event::Role::Assistant,
        text: "",
    }]);
    let request = crate::Request::of(&prompt, "m", 8).expect("a request");
    let body = encode(&request);
    assert!(
        body.contains(r#"{"role":"assistant","content":"","tool_calls":[{"id":"t:0","type":"function","function":{"name":"shell","arguments":"{\"cmd\":\"date\"}"}}]}"#),
        "{body}"
    );
}

#[test]
fn a_tool_message_carries_the_id_it_answers() {
    let prompt = crate::tests::prompt(&[agent_prompt::Message {
        call: Some("t:0"),
        calls: &[],
        role: agent_core::event::Role::Tool,
        text: "noon",
    }]);
    let request = crate::Request::of(&prompt, "m", 8).expect("a request");
    let body = encode(&request);
    assert!(
        body.contains(r#"{"role":"tool","content":"noon","tool_call_id":"t:0"}"#),
        "{body}"
    );
}
