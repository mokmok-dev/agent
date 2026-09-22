//! End-to-end coverage of `agentd-approve`, the client that answers the
//! requests no one has decided.
//!
//! The test launches the real binary against a live daemon: a read token lists
//! the pending `sandbox.permission.requested`, `session.permission.requested`,
//! and `session.egress.requested` records, and the authority token publishes the
//! decision under the same `request_id`, which the durable log then holds. The
//! request events are built with the producers' own constants, so this also
//! fails if the approver's restatement of a type string drifts from the daemon's.
//!
//! The helpers use `expect` like the other integration tests; the workspace
//! `allow-*-in-tests` clippy configuration does not see integration test files,
//! so it is replicated here.
#![expect(
    clippy::expect_used,
    reason = "integration tests use expect for setup and assertions"
)]

use agentd::auth::{Claim, Principal, Token, TokenStore};
use agentd::server;
use agentd_events::{Event, EventLog};
use agentd_inference::{FakeProvider, Provider};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::Arc;
use tokio::net::UnixListener;

/// The token the approver reads with: read only.
const READ_TOKEN: &str = "read-secret";
/// The token the approver decides with: read and authority.
const AUTHORITY_TOKEN: &str = "authority-secret";

/// The token store granting the two clients their capabilities.
fn tokens() -> TokenStore {
    TokenStore::new(vec![
        Token {
            secret: READ_TOKEN.into(),
            principal: Principal::new("urn:test:reader", [Claim::Read]),
        },
        Token {
            secret: AUTHORITY_TOKEN.into(),
            principal: Principal::new(
                "urn:test:approver",
                [Claim::Read, Claim::Publish, Claim::Authority],
            ),
        },
    ])
}

/// A workspace binary from this crate's build output.
///
/// A test runs from `target/<profile>/deps`, so the binary is two directories
/// up. `cargo test --workspace` builds it; a narrower test run may not, which
/// the assertion says rather than failing obscurely.
fn binary(name: &str) -> PathBuf {
    let exe = std::env::current_exe().expect("the test's own path");
    let candidate = exe
        .parent()
        .and_then(Path::parent)
        .expect("the build directory")
        .join(name);
    assert!(
        candidate.is_file(),
        "{} is missing; build the workspace binaries first (cargo build --workspace)",
        candidate.display()
    );
    candidate
}

/// Runs `agentd-approve` with `args` to completion.
fn approve(args: &[&str]) -> Output {
    std::process::Command::new(binary("agentd-approve"))
        .args(args)
        .output()
        .expect("the approver should run")
}

/// Asserts the command succeeded, quoting its stderr when it did not, and
/// returns its stdout.
fn succeeded(output: &Output) -> String {
    assert!(
        output.status.success(),
        "the approver failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The request ids `agentd-approve pending` reports for `connection`.
fn pending_requests(connection: &[&str]) -> Vec<String> {
    let output = approve(&[&["pending"][..], connection].concat());
    succeeded(&output)
        .lines()
        .filter_map(|line| {
            line.split_whitespace()
                .find_map(|field| field.strip_prefix("request_id="))
                .map(str::to_string)
        })
        .collect()
}

/// The argv of `agentd-approve decide` with the fixture's tokens.
fn decide_args<'a>(
    connection: &[&'a str],
    authority_token: &'a str,
    args: &[&'a str],
) -> Vec<&'a str> {
    [
        &["decide"][..],
        connection,
        &["--authority-token-file", authority_token][..],
        args,
    ]
    .concat()
}

/// The stdout of `agentd-approve decide` with the fixture's tokens.
fn decide(
    connection: &[&str],
    authority_token: &str,
    args: &[&str],
) -> String {
    succeeded(&approve(&decide_args(connection, authority_token, args)))
}

/// The decision records in the log for `request_id`.
fn decisions(
    path: &Path,
    request_id: &str,
) -> Vec<Value> {
    std::fs::read_to_string(path)
        .expect("the log should be readable")
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("a log line"))
        .filter(|value| {
            value["data"]["request_id"].as_str() == Some(request_id)
                && value["type"].as_str().is_some_and(|r#type| {
                    r#type.ends_with(".granted")
                        || r#type.ends_with(".denied")
                        || r#type.ends_with(".decided")
                })
        })
        .collect()
}

/// Spawns the daemon's router on `socket`.
fn spawn_server(
    socket: &Path,
    log: EventLog,
    provider: Arc<dyn Provider>,
) -> tokio::task::JoinHandle<()> {
    let listener = UnixListener::bind(socket).expect("listener should bind");
    tokio::spawn(async move {
        axum::serve(listener, server::router(log, tokens(), provider))
            .await
            .expect("server should run");
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_approver_lists_pending_requests_and_answers_them() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("agentd.sock");
    let log_path = dir.path().join("events.jsonl");
    let read_token = dir.path().join("user.token");
    let authority_token = dir.path().join("admin.token");
    std::fs::write(&read_token, READ_TOKEN).expect("the read token");
    std::fs::write(&authority_token, AUTHORITY_TOKEN).expect("the authority token");

    let log = EventLog::open(&log_path).expect("log should open");
    let server = spawn_server(&socket, log.clone(), Arc::new(FakeProvider::default()));

    // The three requests an approver is asked about, built with the producers'
    // own constants: a confined command, a bridged agent's tool call, and a
    // destination a session wants to reach.
    let sandbox = agentd_sandbox::permission_requested(
        "sandbox-1",
        "4",
        "urn:test:agent",
        "rm -rf /tmp/x",
        agentd_sandbox::DECISION_PENDING,
    );
    let session = Event::new(
        agentd::bridge::SESSION_PERMISSION_REQUESTED,
        json!({
            "protocol": "acp",
            "session_id": "agent",
            "request_id": "5",
            "tool_call": { "toolCallId": "call_1" },
            "options": [{ "optionId": "allow-once", "name": "Allow", "kind": "allow_once" }],
        }),
    )
    .with_subject("session:agent");
    let egress = Event::new(
        agentd::proxy::EGRESS_REQUESTED,
        json!({ "request_id": "9", "host": "api.example.com", "port": 443 }),
    );
    for event in [sandbox, session, egress] {
        log.publish(event).await.expect("publish a request");
    }

    let socket_arg = socket.to_string_lossy().into_owned();
    let read_arg = read_token.to_string_lossy().into_owned();
    let authority_arg = authority_token.to_string_lossy().into_owned();
    let connection = ["--socket", socket_arg.as_str(), "--token-file", read_arg.as_str()];

    // The first launch: every unanswered request is reported, in log order.
    assert_eq!(pending_requests(&connection), ["4", "5", "9"]);

    // A session permission is granted by naming one of the options the agent
    // offered, and the decision is published with the authority token.
    let decided = decide(
        &connection,
        &authority_arg,
        &["--request-id", "5", "--granted", "--option-id", "allow-once"],
    );
    assert!(
        decided.contains("decided session.permission.decided request_id=5 outcome=granted"),
        "{decided}"
    );
    let recorded = decisions(&log_path, "5");
    assert_eq!(recorded.len(), 1, "{recorded:?}");
    assert_eq!(recorded[0]["data"]["option_id"], "allow-once");
    assert_eq!(recorded[0]["subject"], "session:agent");

    // The second launch: the answered request is gone, the others remain.
    assert_eq!(pending_requests(&connection), ["4", "9"]);

    // The two other kinds are answered under their own ids.
    let decided = decide(&connection, &authority_arg, &["--request-id", "9", "--denied"]);
    assert!(
        decided.contains("decided session.egress.denied request_id=9 outcome=denied"),
        "{decided}"
    );
    let decided = decide(&connection, &authority_arg, &["--request-id", "4", "--granted"]);
    assert!(
        decided.contains("decided sandbox.permission.granted request_id=4"),
        "{decided}"
    );

    assert_eq!(decisions(&log_path, "9").len(), 1);
    let sandbox_decision = decisions(&log_path, "4");
    assert_eq!(sandbox_decision.len(), 1);
    assert_eq!(sandbox_decision[0]["data"]["decision"], "granted");
    assert_eq!(sandbox_decision[0]["data"]["command"], "rm -rf /tmp/x");

    // Nothing awaits a decision any more.
    assert!(pending_requests(&connection).is_empty());

    // The listing is not the decision: a read token cannot publish one, because
    // every request and decision type is reserved to an authority publisher.
    let refused = approve(&decide_args(
        &connection,
        &read_arg,
        &["--request-id", "5", "--granted", "--option-id", "allow-once"],
    ));
    assert!(!refused.status.success(), "a read token cannot decide");
    assert_eq!(
        decisions(&log_path, "5").len(),
        1,
        "a refused decision must not be recorded"
    );

    server.abort();
}
