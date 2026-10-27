#![allow(
    clippy::expect_used,
    reason = "a test reports a broken assumption by panicking"
)]

use std::fmt::Write as _;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU32, Ordering};

use agent_core::event::{Event, EventKind};
use agent_core::ids::{Actor, BranchId};
use agent_core::time::Timestamp;
use agent_store::Store;

const BIN: &str = env!("CARGO_BIN_EXE_agentd");
const ROOT: BranchId = BranchId::from_bytes([0; 8]);

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn scratch(name: &str) -> PathBuf {
    let unique = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("agentd-{name}-{}-{unique}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("creates scratch dir");
    dir
}

/// Answers `count` requests, one after another, and keeps what it was asked.
fn serve_times(count: usize) -> (String, std::thread::JoinHandle<Vec<Vec<u8>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("binds a port");
    let address = listener.local_addr().expect("an address");
    let handle = std::thread::spawn(move || {
        let mut seen = Vec::with_capacity(count);
        for reply in 0..count {
            let (mut socket, _) = listener.accept().expect("accepts a connection");
            seen.push(read_request(&mut socket));
            let answer = format!("answer-{reply}");
            let frames = [
                format!(r#"{{"choices":[{{"delta":{{"content":"{answer}"}}}}]}}"#),
                r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#.to_string(),
                r#"{"choices":[],"usage":{"prompt_tokens":7,"completion_tokens":2}}"#.to_string(),
                "[DONE]".to_string(),
            ];
            write_stream(&mut socket, &frames);
        }
        seen
    });
    (format!("http://{address}"), handle)
}

fn read_request(socket: &mut std::net::TcpStream) -> Vec<u8> {
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

fn write_stream(
    socket: &mut std::net::TcpStream,
    frames: &[String],
) {
    let mut response = String::from(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n",
    );
    for frame in frames {
        let data = format!("data: {frame}\n\n");
        let length = data.len();
        write!(response, "{length:x}\r\n{data}\r\n").expect("writing to a string cannot fail");
    }
    response.push_str("0\r\n\r\n");
    socket.write_all(response.as_bytes()).expect("writes");
    socket.flush().expect("flushes");
}

/// Answers one request with an event stream, then closes.
fn serve() -> (String, std::thread::JoinHandle<Vec<u8>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("binds a port");
    let address = listener.local_addr().expect("an address");
    let handle = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("accepts one connection");
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
        let frames = [
            r#"{"choices":[{"delta":{"content":"hi"}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            r#"{"choices":[],"usage":{"prompt_tokens":7,"completion_tokens":2}}"#,
            "[DONE]",
        ];
        let mut response = String::from(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n",
        );
        for frame in frames {
            let data = format!("data: {frame}\n\n");
            let length = data.len();
            write!(response, "{length:x}\r\n{data}\r\n").expect("writing to a string cannot fail");
        }
        response.push_str("0\r\n\r\n");
        socket.write_all(response.as_bytes()).expect("writes");
        socket.flush().expect("flushes");
        request
    });
    (format!("http://{address}"), handle)
}

fn write_config(
    dir: &Path,
    base_url: &str,
) -> PathBuf {
    let path = dir.join("agent.toml");
    let text = format!(
        r#"
state_dir = {state:?}
model = "test-model"
base_url = {base:?}
directive_version = "2026-10-26"
directive = "You are a careful agent."
max_output_tokens = 64
timeout_seconds = 5

[[tools]]
argv = ["cat"]
name = "shell"
risk = "read"
schema = '{{"argv":"string"}}'
timeout_ms = 5000
max_output_bytes = 4096

[tools.permissions]
env = []
network = "any"
memory_bytes = 268435456
file_bytes = 8388608

"#,
        state = dir.to_string_lossy(),
        base = base_url
    );
    std::fs::write(&path, text).expect("writes the configuration");
    path
}

fn run(config: &Path) -> Output {
    Command::new(BIN)
        .arg("--config")
        .arg(config)
        .arg("--once")
        .output()
        .expect("runs the daemon")
}

fn append_command(
    dir: &Path,
    text: &str,
) {
    let mut store = Store::open(&dir.join("agent.db")).expect("opens");
    let events = store.load(ROOT).expect("loads");
    let command = Event {
        branch: ROOT,
        depth: u64::try_from(events.len()).expect("small"),
        prev: events.last().map(Event::id),
        time: Timestamp::new(1_790_000_000, 0).expect("valid"),
        actor: Actor::Human("ana".to_string()),
        causation: None,
        correlation: None,
        model: None,
        kind: EventKind::CommandReceived {
            command: "send".to_string(),
            command_id: "c-1".to_string(),
            surface: "test".to_string(),
            text: Some(text.to_string()),
        },
    };
    store.append(&command).expect("appends");
}

#[test]
fn a_configured_daemon_answers_a_turn() {
    let dir = scratch("answer");
    let (base_url, server) = serve();
    let config = write_config(&dir, &base_url);

    let first = run(&config);
    assert!(
        first.status.success(),
        "the first pass starts the store: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    append_command(&dir, "hello");

    let output = run(&config);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let request = String::from_utf8_lossy(&server.join().expect("the server finished")).to_string();
    assert!(
        request.contains(r#""model":"test-model""#),
        "the configured model reaches the provider: {request}"
    );

    let store = Store::open(&dir.join("agent.db")).expect("opens");
    let events = store.load(ROOT).expect("loads");
    let kinds: Vec<&str> = events.iter().map(Event::type_name).collect();
    assert_eq!(
        kinds,
        vec![
            "dev.mokmok.agent.branch.created",
            "dev.mokmok.agent.instance.started",
            "dev.mokmok.agent.command.received",
            "dev.mokmok.agent.turn.requested",
            "dev.mokmok.agent.model.requested",
            "dev.mokmok.agent.model.responded",
            "dev.mokmok.agent.message.appended",
        ]
    );
    match &events[4].kind {
        EventKind::ModelRequested {
            segments,
            tokens,
            warm,
            ..
        } => {
            assert_eq!(*segments, 3, "the directive, the tools, and the input");
            assert!(*tokens > 0);
            assert!(!*warm);
        },
        other => panic!("unexpected {other:?}"),
    }
    match &events[5].kind {
        EventKind::ModelResponded {
            output,
            input_tokens,
            output_tokens,
            ..
        } => {
            assert_eq!(output, "hi");
            assert_eq!(*input_tokens, 7);
            assert_eq!(*output_tokens, 2);
        },
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(store.verify().expect("verifies").events, 7);
}

#[test]
fn a_configuration_that_names_no_model_is_refused_at_startup() {
    let dir = scratch("bad-config");
    let path = dir.join("agent.toml");
    std::fs::write(
        &path,
        format!(
            "state_dir = {:?}\nbase_url = \"http://127.0.0.1:1\"\ndirective = \"x\"\ndirective_version = \"1\"\n",
            dir.to_string_lossy()
        ),
    )
    .expect("writes");

    let output = run(&path);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("model is required"), "{stderr}");
    assert!(
        !dir.join("agent.db").exists(),
        "nothing is opened before the configuration is checked"
    );
}

#[test]
fn a_key_that_is_not_in_the_environment_is_refused_at_startup() {
    let dir = scratch("no-key");
    let path = dir.join("agent.toml");
    std::fs::write(
        &path,
        format!(
            "state_dir = {:?}\nmodel = \"m\"\nbase_url = \"http://127.0.0.1:1\"\ndirective = \"x\"\ndirective_version = \"1\"\napi_key_env = \"AGENT_TEST_KEY_THAT_IS_NOT_SET\"\n",
            dir.to_string_lossy()
        ),
    )
    .expect("writes");

    let output = run(&path);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("AGENT_TEST_KEY_THAT_IS_NOT_SET"),
        "{stderr}"
    );
}

#[test]
fn a_second_turn_sees_the_first_answer() {
    let dir = scratch("history");
    let (base_url, server) = serve_times(2);
    let config = write_config(&dir, &base_url);

    let first = run(&config);
    assert!(
        first.status.success(),
        "the first pass starts the store: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    append_command(&dir, "first question");
    assert!(run(&config).status.success(), "the first turn is answered");
    append_command(&dir, "second question");
    assert!(run(&config).status.success(), "the second turn is answered");

    let requests = server.join().expect("the server finished");
    assert_eq!(requests.len(), 2);
    let second = String::from_utf8_lossy(&requests[1]).to_string();
    assert!(
        second.contains("answer-0"),
        "the first answer is in the second request: {second}"
    );
    assert!(second.contains("first question"), "{second}");
    assert!(second.contains("second question"), "{second}");

    let store = Store::open(&dir.join("agent.db")).expect("opens");
    let events = store.load(ROOT).expect("loads");
    let answers: Vec<&str> = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::MessageAppended { content, role, .. }
                if *role == agent_core::event::Role::Assistant =>
            {
                Some(content.as_str())
            },
            _ => None,
        })
        .collect();
    assert_eq!(answers, vec!["answer-0", "answer-1"]);
    assert_eq!(store.verify().expect("verifies").events, 12);
}

/// Asks for a tool on the first request, then answers with words on the second.
fn serve_tool_call() -> (String, std::thread::JoinHandle<Vec<Vec<u8>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("binds a port");
    let address = listener.local_addr().expect("an address");
    let handle = std::thread::spawn(move || {
        let mut seen = Vec::new();
        let (mut socket, _) = listener.accept().expect("accepts a connection");
        seen.push(read_request(&mut socket));
        let frames = [
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"shell","arguments":"{\"cmd\":"}}]}}]}"#.to_string(),
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"date\"}"}}]}}]}"#
                .to_string(),
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#.to_string(),
            r#"{"choices":[],"usage":{"prompt_tokens":7,"completion_tokens":2}}"#.to_string(),
            "[DONE]".to_string(),
        ];
        write_stream(&mut socket, &frames);

        // The result comes back and the model answers with words.
        let (mut socket, _) = listener.accept().expect("accepts a second connection");
        seen.push(read_request(&mut socket));
        let frames = [
            r#"{"choices":[{"delta":{"content":"it is noon"}}]}"#.to_string(),
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#.to_string(),
            r#"{"choices":[],"usage":{"prompt_tokens":20,"completion_tokens":4}}"#.to_string(),
            "[DONE]".to_string(),
        ];
        write_stream(&mut socket, &frames);
        seen
    });
    (format!("http://{address}"), handle)
}

#[test]
fn a_tool_call_becomes_an_intent_with_a_key() {
    let dir = scratch("tool-call");
    let (base_url, server) = serve_tool_call();
    let config = write_config(&dir, &base_url);

    let first = run(&config);
    assert!(
        first.status.success(),
        "the first pass starts the store: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    append_command(&dir, "what time is it");
    assert!(run(&config).status.success(), "the turn is answered");

    let store = Store::open(&dir.join("agent.db")).expect("opens");
    let events = store.load(ROOT).expect("loads");
    let kinds: Vec<&str> = events.iter().map(|event| event.kind.type_name()).collect();
    let requests = server.join().expect("the server finished");
    assert_eq!(requests.len(), 2, "the result goes back: {kinds:?}");
    assert_eq!(
        kinds,
        vec![
            "dev.mokmok.agent.branch.created",
            "dev.mokmok.agent.instance.started",
            "dev.mokmok.agent.command.received",
            "dev.mokmok.agent.turn.requested",
            "dev.mokmok.agent.model.requested",
            "dev.mokmok.agent.model.responded",
            "dev.mokmok.agent.tool.intent",
            "dev.mokmok.agent.tool.result",
            "dev.mokmok.agent.model.requested",
            "dev.mokmok.agent.model.responded",
            "dev.mokmok.agent.message.appended",
        ]
    );

    // The second request carries the pair the provider joins by id: the answer that
    // asked, with its calls, and the tool's output addressed to one of them.
    let second = String::from_utf8_lossy(&requests[1]).to_string();
    assert!(second.contains(r#""tool_calls":[{"id":""#), "{second}");
    assert!(second.contains(r#""type":"function""#), "{second}");
    assert!(second.contains(r#""tool_call_id":"#), "{second}");
    assert!(second.contains(r#""role":"tool""#), "{second}");
    assert_eq!(events[6].kind.type_name(), "dev.mokmok.agent.tool.intent");
    match &events[7].kind {
        EventKind::ToolResult {
            call,
            output,
            status,
            truncated,
            ..
        } => {
            assert_eq!(
                *call,
                agent_core::ids::CallId {
                    turn: events[3].correlation.expect("a turn"),
                    step: 0,
                }
            );
            assert_eq!(*status, 0);
            assert!(!truncated);
            assert_eq!(
                output.trim(),
                r#"{"cmd":"date"}"#,
                "the arguments went to stdin"
            );
        },
        other => panic!("unexpected {other:?}"),
    }

    match &events[5].kind {
        EventKind::ModelResponded {
            calls,
            finish,
            output,
            ..
        } => {
            assert_eq!(*finish, agent_core::event::FinishReason::ToolCalls);
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].tool, "shell");
            assert_eq!(calls[0].args, r#"{"cmd":"date"}"#);
            assert!(output.is_empty(), "the request is not an answer");
        },
        other => panic!("unexpected {other:?}"),
    }
    match &events[6].kind {
        EventKind::ToolIntent {
            args,
            call,
            env,
            key,
            tool,
        } => {
            assert_eq!(tool, "shell");
            assert!(
                env.is_empty(),
                "the fixture declares no environment, so the intent names none"
            );
            assert_eq!(args, r#"{"cmd":"date"}"#);
            assert_eq!(call.step, 0);
            assert_eq!(call.turn, events[3].correlation.expect("a turn"));
            assert_eq!(key.len(), 64, "a key is a digest, in hex");
        },
        other => panic!("unexpected {other:?}"),
    }
}
