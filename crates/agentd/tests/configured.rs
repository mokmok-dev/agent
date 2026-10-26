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
name = "shell"
schema = '{{"argv":"string"}}'
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

fn append_command(dir: &Path) {
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
            text: Some("hello".to_string()),
        },
    };
    store.append(&command).expect("appends");
}

#[test]
fn a_configured_daemon_answers_a_turn() {
    let dir = scratch("answer");
    let (base_url, server) = serve();
    let config = write_config(&dir, &base_url);

    assert!(
        run(&config).status.success(),
        "the first pass starts the store"
    );
    append_command(&dir);

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
    assert_eq!(store.verify().expect("verifies").events, 6);
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
