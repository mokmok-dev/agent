#![allow(
    clippy::expect_used,
    reason = "a test reports a broken assumption by panicking"
)]

use std::fmt::Write as _;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread::JoinHandle;
use std::time::Duration;

use agent_core::event::{FinishReason, Role};

use super::HttpProvider;
use crate::openai;
use crate::{Completion, Delta, Message, Profile, Provider, ProviderError, Request};

const TIMEOUT: Duration = Duration::from_secs(5);

struct Served {
    base: String,
    handle: std::sync::Mutex<Option<JoinHandle<Vec<u8>>>>,
}

/// Answers exactly one request with the bytes given, then closes.
fn serve(response: String) -> Served {
    let listener = TcpListener::bind("127.0.0.1:0").expect("binds a port");
    let address = listener.local_addr().expect("an address");
    let handle = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("accepts one connection");
        let request = read_head(&mut socket);
        socket.write_all(response.as_bytes()).expect("writes");
        socket.flush().expect("flushes");
        request
    });
    Served {
        base: format!("http://{address}"),
        handle: std::sync::Mutex::new(Some(handle)),
    }
}

/// Reads the head, then exactly as much body as the head declared.
///
/// Reading to end of file would deadlock: the client waits for the response
/// before closing its side, and the server would be waiting for the close.
fn read_head(socket: &mut std::net::TcpStream) -> Vec<u8> {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 4096];
    let head_end = loop {
        let read = socket.read(&mut buffer).expect("reads the request");
        if read == 0 {
            return request;
        }
        request.extend_from_slice(&buffer[..read]);
        if let Some(at) = request.windows(4).position(|window| window == b"\r\n\r\n") {
            break at + 4;
        }
    };
    let head = String::from_utf8_lossy(&request[..head_end]).to_lowercase();
    let declared = head
        .lines()
        .find_map(|line| {
            line.strip_prefix("content-length:")
                .and_then(|value| value.trim().parse::<usize>().ok())
        })
        .unwrap_or(0);
    while request.len() < head_end + declared {
        let read = socket.read(&mut buffer).expect("reads the body");
        if read == 0 {
            break;
        }
        request.extend_from_slice(&buffer[..read]);
    }
    request
}

/// An event-stream response, framed in chunks, closed at the end.
fn stream(frames: &[&str]) -> String {
    let mut response = String::from(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n",
    );
    for frame in frames {
        let data = format!("data: {frame}\n\n");
        let length = data.len();
        write!(response, "{length:x}\r\n{data}\r\n").expect("a write to a string cannot fail");
    }
    response.push_str("0\r\n\r\n");
    response
}

/// Accepts one connection and never answers, so the client's deadline fires.
fn serve_silence() -> Served {
    let listener = TcpListener::bind("127.0.0.1:0").expect("binds a port");
    let address = listener.local_addr().expect("an address");
    let handle = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("accepts one connection");
        let request = read_head(&mut socket);
        std::thread::sleep(Duration::from_millis(300));
        request
    });
    Served {
        base: format!("http://{address}"),
        handle: std::sync::Mutex::new(Some(handle)),
    }
}

fn status(code: u16) -> String {
    format!("HTTP/1.1 {code} Nope\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
}

fn provider(served: &Served) -> HttpProvider {
    HttpProvider::new(
        Profile {
            id: "test".to_string(),
            context_window: 8192,
            caches_prefixes: true,
            cache_ttl: None,
            reports_cached_tokens: true,
        },
        &served.base,
        Some("k-abc".to_string()),
        TIMEOUT,
    )
}

fn request() -> Request {
    Request {
        model: "deepseek".to_string(),
        directive: "be careful".to_string(),
        tools: String::new(),
        messages: vec![Message {
            call: None,
            calls: Vec::new(),
            role: Role::User,
            content: "hello".to_string(),
        }],
        max_output_tokens: 64,
        input_tokens_estimate: 40,
    }
}

fn join(served: &Served) -> Vec<u8> {
    let handle = served
        .handle
        .lock()
        .expect("the handle is not poisoned")
        .take()
        .expect("the server thread was not joined before");
    handle.join().expect("the server thread finished")
}

fn call(served: &Served) -> (Result<Completion, ProviderError>, String, Vec<u8>) {
    let mut adapter = provider(served);
    let mut text = String::new();
    let result = adapter.call(&request(), &mut |delta| match delta {
        Delta::Text(piece) => text.push_str(piece),
    });
    let captured = join(served);
    (result, text, captured)
}

#[test]
fn a_stream_becomes_text_then_usage() {
    let served = serve(stream(&[
        r#"{"choices":[{"delta":{"content":"he"}}]}"#,
        r#"{"choices":[{"delta":{"content":"llo"}}]}"#,
        r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
        r#"{"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":2,"prompt_tokens_details":{"cached_tokens":90}}}"#,
        openai::DONE,
    ]));
    let (result, text, _) = call(&served);
    let completion = result.expect("completes");
    assert_eq!(text, "hello");
    assert_eq!(completion.finish, Some(FinishReason::Stop));
    assert_eq!(completion.usage.input_tokens, 100);
    assert_eq!(completion.usage.output_tokens, 2);
    assert_eq!(completion.usage.cached_tokens, Some(90));
    assert!(!completion.usage.estimated);
}

#[test]
fn a_stream_of_tool_calls_arrives_whole() {
    let served = serve(stream(&[
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"shell","arguments":"{\"cmd\":"}}]}}]}"#,
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"date\"}"}}]}}]}"#,
        r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
        r#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":3}}"#,
        openai::DONE,
    ]));
    let (result, text, _) = call(&served);
    let completion = result.expect("completes");
    assert_eq!(text, "", "a request for a tool is not text");
    assert_eq!(completion.finish, Some(FinishReason::ToolCalls));
    assert_eq!(completion.calls.len(), 1);
    assert_eq!(completion.calls[0].name, "shell");
    assert_eq!(completion.calls[0].arguments, r#"{"cmd":"date"}"#);
}

#[test]
fn a_stream_without_usage_is_marked_estimated() {
    let served = serve(stream(&[
        r#"{"choices":[{"delta":{"content":"hello"}}]}"#,
        r#"{"choices":[{"delta":{},"finish_reason":"length"}]}"#,
        openai::DONE,
    ]));
    let (result, text, _) = call(&served);
    let completion = result.expect("completes");
    assert_eq!(text, "hello");
    assert_eq!(completion.finish, Some(FinishReason::Length));
    assert!(completion.usage.estimated);
    assert_eq!(completion.usage.input_tokens, 40, "the renderer's estimate");
    assert_eq!(completion.usage.output_tokens, 2, "five bytes, rounded up");
    assert_eq!(completion.usage.cached_tokens, None);
}

#[test]
fn a_stream_that_ends_on_a_finish_reason_without_the_sentinel_still_completes() {
    let served = serve(stream(&[
        r#"{"choices":[{"delta":{"content":"hi"}}]}"#,
        r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
    ]));
    let (result, _, _) = call(&served);
    assert_eq!(result.expect("completes").finish, Some(FinishReason::Stop));
}

#[test]
fn a_stream_that_stops_before_a_finish_is_a_failure_after_the_first_token() {
    let served = serve(stream(&[r#"{"choices":[{"delta":{"content":"par"}}]}"#]));
    let (result, text, _) = call(&served);
    assert_eq!(text, "par");
    match result {
        Err(ProviderError::AfterFirstToken(detail)) => {
            assert!(
                detail.contains("finish"),
                "the reason names the cause: {detail}"
            );
        },
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn a_stream_that_dies_before_any_token_is_a_transport_failure() {
    let served = serve(stream(&[]));
    let (result, text, _) = call(&served);
    assert_eq!(text, "");
    match result {
        Err(error @ ProviderError::Transport(_)) => {
            assert_eq!(error.class(), agent_core::event::ErrorClass::Transport);
            assert!(
                !error.detail().is_empty(),
                "the socket said something about why"
            );
        },
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn a_refusal_becomes_the_status_code() {
    let served = serve(status(500));
    let (result, _, _) = call(&served);
    assert_eq!(result, Err(ProviderError::Status(500)));
}

#[test]
fn a_rate_limit_becomes_its_own_status() {
    let served = serve(status(429));
    let (result, _, _) = call(&served);
    assert_eq!(result, Err(ProviderError::Status(429)));
}

#[test]
fn the_request_carries_the_key_the_body_and_the_accept_header() {
    let served = serve(stream(&[openai::DONE]));
    let (_, _, captured) = call(&served);
    let text = String::from_utf8_lossy(&captured);
    let head = text.to_lowercase();
    assert!(head.starts_with("post /chat/completions"), "{head}");
    assert!(head.contains("accept: text/event-stream"), "{head}");
    let authorization = head
        .lines()
        .find(|line| line.starts_with("authorization:"))
        .map(str::trim)
        .expect("an authorization header");
    assert_eq!(authorization, "authorization: bearer k-abc");
    assert!(text.contains(r#""model":"deepseek""#), "{text}");
    assert!(text.contains(r#""stream":true"#), "{text}");
}

#[test]
fn a_base_url_with_a_trailing_slash_does_not_double_it() {
    let served = serve(stream(&[openai::DONE]));
    let mut adapter = HttpProvider::new(
        Profile {
            id: "test".to_string(),
            context_window: 1,
            caches_prefixes: false,
            cache_ttl: None,
            reports_cached_tokens: false,
        },
        &format!("{}/", served.base),
        None,
        TIMEOUT,
    );
    let mut text = String::new();
    let result = adapter.call(&request(), &mut |delta| match delta {
        Delta::Text(piece) => text.push_str(piece),
    });
    assert!(result.is_ok());
    let captured = join(&served);
    let head = String::from_utf8_lossy(&captured).to_lowercase();
    assert!(head.starts_with("post /chat/completions"), "{head}");
    assert!(!head.contains("authorization"), "no key was given: {head}");
}

#[test]
fn a_request_that_never_answers_is_a_timeout() {
    let served = serve_silence();
    let mut adapter = HttpProvider::new(
        Profile {
            id: "test".to_string(),
            context_window: 1,
            caches_prefixes: false,
            cache_ttl: None,
            reports_cached_tokens: false,
        },
        &served.base,
        None,
        Duration::from_millis(50),
    );
    let mut text = String::new();
    let result = adapter.call(&request(), &mut |delta| match delta {
        Delta::Text(piece) => text.push_str(piece),
    });
    assert_eq!(result, Err(ProviderError::Timeout));
    let _ = join(&served);
}
