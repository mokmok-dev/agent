use agent_core::event::{FinishReason, Role};

use super::{Chunk, CodecError, DONE, Usage, decode, encode, finish_of};
use crate::{Message, Request};

fn request() -> Request {
    Request {
        model: "deepseek".to_string(),
        directive: "You are a careful agent.".to_string(),
        tools: "4:read17:{\"path\":\"string\"}".to_string(),
        messages: vec![
            Message {
                role: Role::User,
                content: "hello".to_string(),
            },
            Message {
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
fn the_directive_comes_first_as_a_system_message_and_the_tools_second() {
    let body = encode(&request());
    let first = body.find(r#""role":"system""#).expect("a system message");
    let tools = body.find("4:read").expect("the tool block");
    let user = body.find(r#""role":"user""#).expect("a user message");
    assert!(first < tools, "{body}");
    assert!(tools < user, "{body}");
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
    bare.tools = String::new();
    let body = encode(&bare);
    assert_eq!(body.matches(r#""role":"system""#).count(), 1, "{body}");
}

#[test]
fn a_quote_in_a_message_is_escaped_rather_than_breaking_the_body() {
    let mut quoted = request();
    quoted.messages[0].content = "he said \"hi\"\n".to_string();
    let body = encode(&quoted);
    assert!(body.contains(r#"he said \"hi\"\n"#), "{body}");
    let parsed: serde_json::Value = serde_json::from_str(&body).expect("valid json");
    assert_eq!(
        parsed["messages"][2]["content"].as_str(),
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
