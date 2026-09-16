//! End-to-end coverage of the agent loop: a user prompt becomes an `agent.inbox`
//! event, the sandboxed agent asks the daemon for a completion, runs the shell
//! tool the model calls, and publishes the finalized messages back to the log.
//!
//! The model is a scripted [`FakeProvider`], so the loop is deterministic and no
//! network is involved. The helpers use `expect`/`panic` like the other
//! integration tests; the workspace `allow-*-in-tests` clippy configuration does
//! not see integration test files, so it is replicated here.

#![allow(clippy::expect_used, clippy::panic)]

use agentd::auth::{Claim, Principal, Token, TokenStore};
use agentd::server;
use agentd_events::{Event, EventLog};
use agentd_inference::{Delta, FakeProvider};
use agentd_node::{Agent, Conversation, Message, ShellLimits, SqliteProjection};
use serde_json::json;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UnixListener;
use tokio::sync::watch;

/// The agent's token secret: read, publish, and infer.
const AGENT_TOKEN: &str = "agent-secret";
/// The user's token secret: read and publish.
const USER_TOKEN: &str = "user-secret";

/// The token store granting the agent and the user their capabilities.
fn tokens() -> TokenStore {
    TokenStore::new(vec![
        Token {
            secret: String::from(AGENT_TOKEN),
            principal: Principal::new(
                "urn:test:agent",
                [Claim::Read, Claim::Publish, Claim::Infer],
            ),
        },
        Token {
            secret: String::from(USER_TOKEN),
            principal: Principal::new("urn:test:user", [Claim::Read, Claim::Publish]),
        },
    ])
}

/// Spawns the daemon with a scripted provider on `socket`.
fn spawn_server(
    socket: &Path,
    log: EventLog,
    provider: Arc<FakeProvider>,
) -> tokio::task::JoinHandle<()> {
    let listener = UnixListener::bind(socket).expect("listener should bind");
    tokio::spawn(async move {
        axum::serve(listener, server::router(log, tokens(), provider))
            .await
            .expect("server should run");
    })
}

/// A provider script that calls `shell` once and then finishes.
fn scripted_provider() -> Arc<FakeProvider> {
    Arc::new(FakeProvider::new(vec![
        vec![
            Delta::ToolCall {
                id: String::from("call-1"),
                name: String::from("shell"),
                arguments: String::from(r#"{"command":"echo hello"}"#),
            },
            Delta::Done {
                finish_reason: Some(String::from("tool_calls")),
            },
        ],
        vec![
            Delta::Text {
                text: String::from("done"),
            },
            Delta::Done {
                finish_reason: Some(String::from("stop")),
            },
        ],
    ]))
}

/// Polls the conversation projection until it holds at least `expected` messages.
async fn wait_for_history(
    db: &Path,
    conversation: &str,
    expected: usize,
) -> Vec<Message> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        if let Ok(projection) = SqliteProjection::<Conversation>::open(db)
            && let Ok(history) = Conversation::history(projection.connection(), conversation)
            && history.len() >= expected
        {
            return history;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {expected} messages"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Reads the `CloudEvents` `type` of every line in the log.
fn logged_types(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .expect("log should be readable")
        .lines()
        .map(|line| {
            let value: serde_json::Value = serde_json::from_str(line).expect("line should decode");
            value["type"]
                .as_str()
                .expect("type should be present")
                .to_string()
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_inbox_runs_a_tool_turn_and_publishes_the_conversation() {
    let dir = tempfile::tempdir().expect("tempdir should be created");
    let socket = dir.path().join("agentd.sock");
    let db = dir.path().join("agent.db");
    let log_path = dir.path().join("events.jsonl");
    let log = EventLog::open(&log_path).expect("log should open");

    let server = spawn_server(&socket, log.clone(), scripted_provider());

    let projection = SqliteProjection::<Conversation>::open(&db).expect("projection should open");
    let mut agent = Agent::new(
        &socket,
        projection,
        "c1",
        dir.path(),
        "urn:test:agent",
        AGENT_TOKEN,
    )
    .with_limits(ShellLimits {
        timeout: Duration::from_secs(10),
        ..ShellLimits::default()
    });
    let (shutdown, receiver) = watch::channel(false);
    let handle = tokio::spawn(async move { agent.run(receiver).await });

    log.publish(Event::new(
        "agent.inbox",
        json!({ "conversation_id": "c1", "content": "say hello" }),
    ))
    .await
    .expect("publish should succeed");

    let history = wait_for_history(&db, "c1", 4).await;

    assert_eq!(history[0], Message::user("say hello"));
    assert_eq!(history[1].role, agentd_inference::Role::Assistant);
    assert_eq!(history[1].tool_calls.len(), 1);
    assert_eq!(history[1].tool_calls[0].name, "shell");
    assert_eq!(history[2].role, agentd_inference::Role::Tool);
    assert!(
        history[2].content.contains("hello"),
        "the tool output should include the command output: {:?}",
        history[2].content
    );
    assert_eq!(history[3], Message::assistant("done"));

    let types = logged_types(&log_path);
    assert!(types.contains(&String::from("agent.tool_result")));
    assert!(types.contains(&String::from("agent.turn.completed")));
    assert!(!types.contains(&String::from("agent.turn.failed")));

    shutdown.send(true).expect("shutdown should be sent");
    handle
        .await
        .expect("agent should join")
        .expect("agent should stop cleanly");
    server.abort();
}
