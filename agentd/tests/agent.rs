//! End-to-end test for the agent program over the real bus.
//!
//! The agent binary runs as its own process, addressed to a session by subject,
//! with the bus server the daemon would run. A test client publishes a command and
//! reads the agent's output, which is the whole interface the design gives the
//! agent.
#![expect(
    clippy::expect_used,
    clippy::panic,
    reason = "integration test code may panic when a fixture fails"
)]

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent::bus::Bus;
use agent::client::{Client, Config as BusConfig, Handler};
use agent::cloudevent::{Event as CloudEvent, Incoming, SpecVersion};
use agent::server::Server;
use agent::transport::{Allowlist, Listener, WebSocketConfig};
use serde_json::json;

/// A running bus server on a fresh socket.
struct BusServer {
    root: PathBuf,
    socket: PathBuf,
    _runtime: tokio::runtime::Runtime,
}

impl BusServer {
    fn start(tag: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("agent-e2e-{tag}-{}-{unique}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("creates the root");
        let socket = root.join("bus.sock");

        let uid = std::fs::metadata(std::env::temp_dir())
            .expect("the temp dir has metadata")
            .uid();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("builds a runtime");
        let guard = runtime.enter();
        let listener = Listener::bind(&socket, WebSocketConfig::default()).expect("binds");
        let bus = Bus::open(root.join("data"), 64).expect("opens the bus");
        let server = Server::new(listener, bus, Allowlist::new().allow_uid(uid));
        runtime.spawn(server.run());
        drop(guard);
        wait_for(&socket);
        Self {
            root,
            socket,
            _runtime: runtime,
        }
    }

    fn client(&self) -> Client {
        Client::connect(&BusConfig {
            socket: self.socket.clone(),
            subscriber_id: "e2e-test".to_owned(),
        })
        .expect("connects")
    }
}

impl Drop for BusServer {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Wait until the socket accepts a connection.
fn wait_for(socket: &Path) {
    for _ in 0..100 {
        if std::os::unix::net::UnixStream::connect(socket).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("the socket never accepted at {}", socket.display());
}

/// An incoming event of `ty`.
fn incoming(
    ty: &str,
    subject: &str,
    data: serde_json::Value,
) -> Incoming {
    Incoming {
        specversion: SpecVersion::V1_0,
        ty: ty.to_owned(),
        source: None,
        id: None,
        time: None,
        subject: Some(subject.to_owned()),
        datacontenttype: None,
        sequence: None,
        data: Some(data),
        extensions: std::collections::BTreeMap::new(),
    }
}

/// A handler that records every event.
fn recorder() -> (Handler, Arc<Mutex<Vec<CloudEvent>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let handler = Box::new(move |event: &CloudEvent| {
        sink.lock().expect("unpoisoned").push(event.clone());
    });
    (handler, seen)
}

/// Poll `seen` for an event matching `predicate`, or fail.
fn await_event<F>(
    seen: &Arc<Mutex<Vec<CloudEvent>>>,
    what: &str,
    predicate: F,
) -> CloudEvent
where
    F: Fn(&CloudEvent) -> bool,
{
    for _ in 0..300 {
        let events = seen.lock().expect("unpoisoned").clone();
        if let Some(event) = events.iter().find(|event| predicate(event)) {
            return event.clone();
        }
        drop(events);
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("never saw {what}");
}

/// A running agent binary, killed on drop.
struct Agent {
    child: Child,
}

impl Agent {
    /// Start the agent for `session`, replaying from `from_seq`.
    fn start(
        server: &BusServer,
        session: &str,
        from_seq: u64,
    ) -> Self {
        let child = Command::new(env!("CARGO_BIN_EXE_agent-agent"))
            .arg("--socket")
            .arg(&server.socket)
            .arg("--session")
            .arg(session)
            .arg("--from-seq")
            .arg(from_seq.to_string())
            .env("RUST_LOG", "info")
            .spawn()
            .expect("the agent binary starts");
        Self { child }
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn an_agent_answers_a_command_with_output_and_acks_it() {
    let server = BusServer::start("echo");
    let observer = server.client();
    let (handler, seen) = recorder();
    observer.subscribe(0, handler).expect("subscribes");

    // The agent replays the whole log, so the command must be in it first.
    let client = server.client();
    client
        .publish(incoming(
            "agent.session.command",
            "s-1",
            json!({"action": "run", "detail": {"script": "echo hi"}}),
        ))
        .expect("publishes the command");

    let _agent = Agent::start(&server, "s-1", 0);

    let output = await_event(&seen, "an output event", |event| {
        event.ty == "agent.session.output" && event.subject.as_deref() == Some("s-1")
    });
    assert_eq!(
        output.data.as_ref().expect("data")["kind"],
        json!("done"),
        "the echo capability reports done"
    );
    assert_eq!(
        output.data.as_ref().expect("data")["action"],
        "run",
        "the action is echoed"
    );
    assert_eq!(
        output.data.as_ref().expect("data")["detail"]["echoed"]["script"],
        "echo hi",
        "the command's detail is echoed back"
    );
}

#[test]
fn an_agent_survives_a_restart_without_losing_a_command() {
    // A command published while no agent is running is durable. An agent that
    // starts later replays it from its cursor, so nothing is lost: this is the
    // property the design gives a bus peer.
    let server = BusServer::start("restart");
    let observer = server.client();
    let (handler, seen) = recorder();
    observer.subscribe(0, handler).expect("subscribes");

    // Publish the command with no agent running.
    let client = server.client();
    client
        .publish(incoming(
            "agent.session.command",
            "s-2",
            json!({"action": "run", "detail": {"task": "wake"}}),
        ))
        .expect("publishes the command");

    // Start the agent after the command exists; it must still receive it.
    let _agent = Agent::start(&server, "s-2", 0);

    let output = await_event(&seen, "the replayed command's output", |event| {
        event.ty == "agent.session.output" && event.subject.as_deref() == Some("s-2")
    });
    assert_eq!(output.data.as_ref().expect("data")["action"], "run");
    assert_eq!(
        output.data.as_ref().expect("data")["detail"]["echoed"]["task"],
        "wake"
    );
}

#[test]
fn an_agent_ignores_a_command_addressed_to_another_session() {
    // Sessions share one bus; the subject is what scopes a command. An agent for
    // `s-3` must not act on a command for `s-4`.
    let server = BusServer::start("scoped");
    let observer = server.client();
    let (handler, seen) = recorder();
    observer.subscribe(0, handler).expect("subscribes");

    let client = server.client();
    client
        .publish(incoming(
            "agent.session.command",
            "s-4",
            json!({"action": "run", "detail": {}}),
        ))
        .expect("publishes a command for another session");

    let _agent = Agent::start(&server, "s-3", 0);
    std::thread::sleep(Duration::from_millis(400));

    assert!(
        !seen
            .lock()
            .expect("unpoisoned")
            .iter()
            .any(|event| event.ty == "agent.session.output"
                && event.subject.as_deref() == Some("s-3")),
        "an agent must not act on another session's command"
    );
}
