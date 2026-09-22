//! End-to-end coverage of the patch tool: a prompt makes the model call
//! `apply_patch`, the agent changes the workspace file, and the change is
//! recorded as an `agent.patch.applied` event carrying the inverse patch.
//!
//! The inverse is then applied through the shipped patcher, which is the undo
//! the log is supposed to make possible: no daemon-side patch store is involved.
//! The model is a scripted [`FakeProvider`], so the loop is deterministic and no
//! network is involved.
//!
//! The helpers use `expect`/`panic` like the other integration tests; the
//! workspace `allow-*-in-tests` clippy configuration does not see integration
//! test files, so it is replicated here.
#![expect(
    clippy::expect_used,
    reason = "integration tests use expect for setup and assertions"
)]

use agentd::auth::{Claim, Principal, Token, TokenStore};
use agentd::server;
use agentd_events::{Event, EventLog};
use agentd_inference::{Delta, FakeProvider, Provider};
use agentd_node::patch::Patch;
use agentd_node::{AGENT_PATCH_APPLIED, Agent, Conversation, Message, SqliteProjection};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UnixListener;
use tokio::sync::watch;

/// The agent's token secret: read, publish, and infer.
const AGENT_TOKEN: &str = "agent-secret";

/// The file the patch edits, relative to the workspace.
const FILE: &str = "notes.txt";

/// The content the workspace file starts with.
const ORIGINAL: &str = "one\ntwo\nthree\n";

/// The diff the model will call the tool with.
const DIFF: &str = "\
--- a/notes.txt
+++ b/notes.txt
@@ -1,3 +1,4 @@
 one
+one and a half
 two
 three
";

/// The token store granting the agent its capabilities.
fn tokens() -> TokenStore {
    TokenStore::new(vec![Token {
        secret: AGENT_TOKEN.into(),
        principal: Principal::new(
            "urn:test:agent",
            [Claim::Read, Claim::Publish, Claim::Infer],
        ),
    }])
}

/// Spawns the daemon with a scripted provider on `socket`.
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

/// A provider script that calls `apply_patch` once and then finishes.
fn scripted_provider() -> Arc<dyn Provider> {
    Arc::new(FakeProvider::new(vec![
        vec![
            Delta::ToolCall {
                id: String::from("call-1"),
                name: String::from("apply_patch"),
                arguments: json!({ "patch": DIFF }).to_string(),
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

/// Polls the conversation projection until it holds at least `expected`
/// messages.
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

/// Every event recorded in the log, read back from the file.
fn recorded(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .expect("the log should be readable")
        .lines()
        .map(|line| serde_json::from_str(line).expect("every line should decode"))
        .collect()
}

/// The single `agent.patch.applied` event the log holds.
fn applied_patch(path: &Path) -> Value {
    let events = recorded(path);
    let patches: Vec<&Value> = events
        .iter()
        .filter(|event| event["type"] == AGENT_PATCH_APPLIED)
        .collect();
    assert_eq!(patches.len(), 1, "{patches:#?}");
    patches[0].clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_patch_tool_call_changes_the_workspace_and_is_reversible_from_the_log() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("agentd.sock");
    let db = dir.path().join("agent.db");
    let log_path = dir.path().join("events.jsonl");
    let log = EventLog::open(&log_path).expect("log should open");
    let server = spawn_server(&socket, log.clone(), scripted_provider());

    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).expect("the workspace");
    let file = workspace.join(FILE);
    std::fs::write(&file, ORIGINAL).expect("the original file");

    let projection = SqliteProjection::<Conversation>::open(&db).expect("projection should open");
    let mut agent = Agent::new(
        &socket,
        projection,
        "c1",
        &workspace,
        "urn:test:agent",
        AGENT_TOKEN,
    );
    let (shutdown, receiver) = watch::channel(false);
    let handle = tokio::spawn(async move { agent.run(receiver).await });

    log.publish(Event::new(
        "agent.inbox",
        json!({ "conversation_id": "c1", "content": "add a line" }),
    ))
    .await
    .expect("publish should succeed");

    let history = wait_for_history(&db, "c1", 4).await;
    assert_eq!(history[1].tool_calls[0].name, "apply_patch");
    assert_eq!(history[3], Message::assistant("done"));

    // The tool changed the file the patch named.
    assert_eq!(
        std::fs::read_to_string(&file).expect("the file should be readable"),
        "one\none and a half\ntwo\nthree\n"
    );

    // The log holds the change with everything an undo needs.
    let applied = applied_patch(&log_path);
    assert_eq!(
        applied["data"]["files"],
        json!([{ "path": FILE, "added": 1, "removed": 0 }]),
        "{applied:#?}"
    );
    let inverse = applied["data"]["inverse"].as_str().expect("an inverse patch");

    // Undo is "read the inverse from the log and apply it": the file goes back
    // to the bytes it had, with nothing but the event.
    let restored = Patch::parse(inverse)
        .expect("the inverse should parse")
        .apply(|path| {
            assert_eq!(path, FILE);
            std::fs::read_to_string(&file)
        })
        .expect("the inverse should apply");
    std::fs::write(&file, &restored[0].content).expect("the undo should write");
    assert_eq!(
        std::fs::read_to_string(&file).expect("the file should be readable"),
        ORIGINAL
    );

    shutdown.send(true).expect("shutdown should be sent");
    handle
        .await
        .expect("agent should join")
        .expect("agent should stop cleanly");
    server.abort();
}
