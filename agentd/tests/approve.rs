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

/// A live daemon, the log it serves, and the two token files it accepts.
struct Fixture {
    _dir: tempfile::TempDir,
    log_path: PathBuf,
    socket: String,
    read_token: String,
    authority_token: String,
    server: tokio::task::JoinHandle<()>,
}

impl Fixture {
    /// Starts a daemon on a socket in a fresh temporary directory.
    fn start() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("agentd.sock");
        let log_path = dir.path().join("events.jsonl");
        let read_token = dir.path().join("user.token");
        let authority_token = dir.path().join("admin.token");
        std::fs::write(&read_token, READ_TOKEN).expect("the read token");
        std::fs::write(&authority_token, AUTHORITY_TOKEN).expect("the authority token");
        let log = EventLog::open(&log_path).expect("log should open");
        let listener = UnixListener::bind(&socket_path).expect("listener should bind");
        let provider: Arc<dyn Provider> = Arc::new(FakeProvider::default());
        let server = tokio::spawn(async move {
            axum::serve(listener, server::router(log, tokens(), provider))
                .await
                .expect("server should run");
        });
        Self {
            _dir: dir,
            log_path,
            socket: socket_path.to_string_lossy().into_owned(),
            read_token: read_token.to_string_lossy().into_owned(),
            authority_token: authority_token.to_string_lossy().into_owned(),
            server,
        }
    }

    /// The connection flags both subcommands take.
    const fn connection(&self) -> [&str; 4] {
        [
            "--socket",
            self.socket.as_str(),
            "--token-file",
            self.read_token.as_str(),
        ]
    }

    /// The request ids `agentd-approve pending` reports.
    fn pending(&self) -> Vec<String> {
        let output = approve(&[&["pending"][..], &self.connection()].concat());
        succeeded(&output)
            .lines()
            .filter_map(|line| {
                line.split_whitespace()
                    .find_map(|field| field.strip_prefix("request_id="))
                    .map(str::to_string)
            })
            .collect()
    }

    /// The stdout of `agentd-approve decide` with `args`.
    fn decide(
        &self,
        authority_token: &str,
        args: &[&str],
    ) -> String {
        succeeded(&approve(&self.decide_args(authority_token, args)))
    }

    /// The argv of `agentd-approve decide` with `args`.
    fn decide_args<'a>(
        &'a self,
        authority_token: &'a str,
        args: &[&'a str],
    ) -> Vec<&'a str> {
        [
            &["decide"][..],
            &self.connection(),
            &["--authority-token-file", authority_token][..],
            args,
        ]
        .concat()
    }

    /// The decision records in the log for `request_id`.
    fn decisions(
        &self,
        request_id: &str,
    ) -> Vec<Value> {
        std::fs::read_to_string(&self.log_path)
            .expect("the log should be readable")
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).expect("a log line"))
            .filter(|value| {
                value["data"]["request_id"].as_str() == Some(request_id)
                    && value["type"].as_str().is_some_and(|r#type| {
                        r#type.ends_with(".granted")
                            || r#type.ends_with(".denied")
                            || r#type.ends_with(".cancelled")
                    })
            })
            .collect()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

/// A bridged agent's request for a tool call, addressed to `session`.
fn session_request(
    request_id: &str,
    session: &str,
) -> Event {
    Event::new(
        agentd::bridge::SESSION_PERMISSION_REQUESTED,
        json!({
            "protocol": "acp",
            "session_id": session,
            "request_id": request_id,
            "tool_call": { "toolCallId": "call_1" },
            "options": [
                { "optionId": "allow-once", "name": "Allow", "kind": "allow_once" },
                { "optionId": "reject-once", "name": "Reject", "kind": "reject_once" },
            ],
        }),
    )
    .with_subject(format!("session:{session}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_approver_lists_pending_requests_and_answers_them() {
    let fixture = Fixture::start();

    // The three requests an approver is asked about, built with the producers'
    // own constants: a confined command, a bridged agent's tool call, and a
    // destination a session wants to reach.
    let log = EventLog::open(&fixture.log_path).expect("log should open");
    for event in [
        agentd_sandbox::permission_requested(
            "sandbox-1",
            "4",
            "urn:test:agent",
            "rm -rf /tmp/x",
            agentd_sandbox::DECISION_PENDING,
        ),
        session_request("5", "agent"),
        Event::new(
            agentd::proxy::EGRESS_REQUESTED,
            json!({ "request_id": "9", "host": "api.example.com", "port": 443 }),
        ),
    ] {
        log.publish(event).await.expect("publish a request");
    }

    // The first launch: every unanswered request is reported, in log order.
    assert_eq!(fixture.pending(), ["4", "5", "9"]);

    // A session permission is granted by naming one of the options the agent
    // offered, and the decision is published with the authority token.
    let decided = fixture.decide(
        &fixture.authority_token,
        &[
            "--request-id",
            "5",
            "--granted",
            "--option-id",
            "allow-once",
        ],
    );
    assert!(
        decided.contains("decided session.permission.granted request_id=5 outcome=granted"),
        "{decided}"
    );
    let recorded = fixture.decisions("5");
    assert_eq!(recorded.len(), 1, "{recorded:?}");
    assert_eq!(recorded[0]["data"]["option_id"], "allow-once");
    assert_eq!(recorded[0]["subject"], "session:agent");

    // The second launch: the answered request is gone, the others remain.
    assert_eq!(fixture.pending(), ["4", "9"]);

    // The two other kinds are answered under their own ids.
    let decided = fixture.decide(&fixture.authority_token, &["--request-id", "9", "--denied"]);
    assert!(
        decided.contains("decided session.egress.denied request_id=9 outcome=denied"),
        "{decided}"
    );
    let decided = fixture.decide(
        &fixture.authority_token,
        &["--request-id", "4", "--granted"],
    );
    assert!(
        decided.contains("decided sandbox.permission.granted request_id=4"),
        "{decided}"
    );

    assert_eq!(fixture.decisions("9").len(), 1);
    let sandbox_decision = fixture.decisions("4");
    assert_eq!(sandbox_decision.len(), 1);
    assert_eq!(sandbox_decision[0]["data"]["decision"], "granted");
    assert_eq!(sandbox_decision[0]["data"]["command"], "rm -rf /tmp/x");

    // A cancellation is its own outcome, not a denial: the operator withdrew
    // the request, and the log must not claim they refused it.
    log.publish(agentd_sandbox::permission_requested(
        "sandbox-1",
        "11",
        "urn:test:agent",
        "ls",
        agentd_sandbox::DECISION_PENDING,
    ))
    .await
    .expect("publish a request");
    assert_eq!(fixture.pending(), ["11"]);
    let cancelled = fixture.decide(
        &fixture.authority_token,
        &["--request-id", "11", "--cancelled"],
    );
    assert!(
        cancelled.contains("decided sandbox.permission.cancelled request_id=11 outcome=cancelled"),
        "{cancelled}"
    );
    let recorded = fixture.decisions("11");
    assert_eq!(recorded.len(), 1, "{recorded:?}");
    assert_eq!(recorded[0]["data"]["decision"], "cancelled");

    // Nothing awaits a decision any more.
    assert!(fixture.pending().is_empty());

    // The listing is not the decision: a read token cannot publish one, because
    // every request and decision type is reserved to an authority publisher.
    let refused = approve(&fixture.decide_args(
        &fixture.read_token,
        &[
            "--request-id",
            "5",
            "--granted",
            "--option-id",
            "allow-once",
        ],
    ));
    assert!(!refused.status.success(), "a read token cannot decide");
    assert_eq!(
        fixture.decisions("5").len(),
        1,
        "a refused decision must not be recorded"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_sessions_asking_under_one_request_id_are_told_apart() {
    let fixture = Fixture::start();
    let log = EventLog::open(&fixture.log_path).expect("log should open");

    // A bridged child numbers its own permission requests, so two sessions ask
    // under the same id; only the subject tells them apart.
    for event in [
        session_request("7", "first"),
        session_request("7", "second"),
    ] {
        log.publish(event).await.expect("publish a request");
    }
    assert_eq!(fixture.pending(), ["7", "7"]);

    // Answering the id alone would leave the other session waiting, so the
    // approver refuses rather than guessing.
    let ambiguous =
        approve(&fixture.decide_args(&fixture.authority_token, &["--request-id", "7", "--denied"]));
    assert!(!ambiguous.status.success(), "an ambiguous id is refused");
    assert!(
        String::from_utf8_lossy(&ambiguous.stderr).contains("pass --subject"),
        "{}",
        String::from_utf8_lossy(&ambiguous.stderr)
    );
    assert!(fixture.decisions("7").is_empty());

    // Naming the session answers exactly that one.
    let decided = fixture.decide(
        &fixture.authority_token,
        &[
            "--request-id",
            "7",
            "--subject",
            "session:second",
            "--denied",
        ],
    );
    assert!(
        decided.contains("decided session.permission.denied request_id=7 outcome=denied"),
        "{decided}"
    );
    let recorded = fixture.decisions("7");
    assert_eq!(recorded.len(), 1, "{recorded:?}");
    assert_eq!(recorded[0]["subject"], "session:second");
    assert_eq!(fixture.pending(), ["7"], "the other session still waits");
}
