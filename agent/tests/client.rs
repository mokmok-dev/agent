//! End-to-end tests for the bus client against the real bus server.
//!
//! The server is the real [`agent::server::Server`] over a real Unix socket, and
//! the client is the one the daemon and the confined agent use, so these exercise
//! the wire protocol both will speak. The client is synchronous, so the tests are
//! plain `#[test]`; the server runs on a runtime in a background thread.
#![expect(
    clippy::expect_used,
    clippy::panic,
    reason = "integration test code may panic when a fixture fails"
)]

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use agent::bus::Bus;
use agent::client::{Client, Config, Handler};
use agent::cloudevent::{Event, Incoming, SpecVersion};
use agent::server::Server;
use agent::transport::{Allowlist, Listener, WebSocketConfig};
use serde_json::json;

/// A running bus server on a fresh socket, with its data directory.
struct BusServer {
    root: PathBuf,
    socket: PathBuf,
    _runtime: tokio::runtime::Runtime,
}

impl BusServer {
    fn start(tag: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "agent-client-{tag}-{}-{unique}",
            std::process::id()
        ));
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
        // `Listener::bind` creates a tokio `UnixListener`, which needs a runtime
        // context, and the accept loop needs a runtime to spawn on. The guard is
        // dropped after the bind; the runtime keeps running the server's tasks.
        let guard = runtime.enter();
        let listener = Listener::bind(&socket, WebSocketConfig::default()).expect("binds");
        let bus = Bus::open(root.join("data"), 16).expect("opens the bus");
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
        Client::connect(&Config {
            socket: self.socket.clone(),
            subscriber_id: "test".to_owned(),
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
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    panic!("the socket never accepted at {}", socket.display());
}

/// An incoming event of `ty` with `data`, as a caller authors one.
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

/// A handler that records every event it receives.
fn recorder() -> (Handler, Arc<Mutex<Vec<Event>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let handler = Box::new(move |event: &Event| {
        sink.lock().expect("unpoisoned").push(event.clone());
    });
    (handler, seen)
}

/// Poll `seen` until it holds at least `count` events, or fail.
fn await_events(
    seen: &Arc<Mutex<Vec<Event>>>,
    count: usize,
) -> Vec<Event> {
    for _ in 0..200 {
        let events = seen.lock().expect("unpoisoned").clone();
        if events.len() >= count {
            return events;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    panic!(
        "only received {} events, wanted {count}",
        seen.lock().expect("unpoisoned").len()
    );
}

#[test]
fn a_published_event_is_committed_and_read_back() {
    let server = BusServer::start("publish");
    let client = server.client();

    let receipt = client
        .publish(incoming(
            "agent.session.started",
            "s-1",
            json!({"workspace": "/work"}),
        ))
        .expect("publishes");
    assert_eq!(receipt.seq, 0, "the first event is sequence zero");

    let (handler, seen) = recorder();
    let from_seq = client.subscribe(0, handler).expect("subscribes");
    assert_eq!(from_seq, 0);

    let events = await_events(&seen, 1);
    let event = &events[0];
    assert_eq!(event.ty, "agent.session.started");
    assert_eq!(event.subject.as_deref(), Some("s-1"));
    assert_eq!(event.data, Some(json!({"workspace": "/work"})));
    assert_eq!(event.source, "agent://eventbus", "the bus owns the source");
    assert_eq!(event.sequence.get(), 0);
}

#[test]
fn a_subscriber_receives_a_live_event() {
    // A second client publishes while the first is subscribed, so the event is
    // delivered live rather than replayed.
    let server = BusServer::start("live");
    let subscriber = server.client();
    let publisher = server.client();

    let (handler, seen) = recorder();
    subscriber.subscribe(0, handler).expect("subscribes");
    publisher
        .publish(incoming(
            "agent.session.exited",
            "s-1",
            json!({"reason": "done"}),
        ))
        .expect("publishes");

    let events = await_events(&seen, 1);
    assert_eq!(events[0].ty, "agent.session.exited");
    assert_eq!(events[0].data, Some(json!({"reason": "done"})));
}

#[test]
fn one_connection_can_publish_and_receive() {
    // One connection is used for both, which is what the daemon and the agent do.
    let server = BusServer::start("both");
    let client = server.client();

    let (handler, seen) = recorder();
    client.subscribe(0, handler).expect("subscribes");
    client
        .publish(incoming("agent.session.stopped", "s-1", json!({})))
        .expect("publishes on the same connection");

    let events = await_events(&seen, 1);
    assert_eq!(events[0].ty, "agent.session.stopped");
}

#[test]
fn a_producer_set_bus_attribute_is_refused() {
    // `source` is the bus's, so a producer that sets it is refused and nothing is
    // committed.
    let server = BusServer::start("reserved");
    let client = server.client();

    let mut forged = incoming("agent.session.started", "s-1", json!({}));
    forged.source = Some("agent://forged".to_owned());

    let error = client
        .publish(forged)
        .expect_err("a reserved attribute fails");
    assert!(
        matches!(
            error,
            agent::client::Error::Refused {
                code: agent::protocol::ErrorCode::ReservedAttribute,
                ..
            }
        ),
        "got {error:?}"
    );

    // Nothing was committed.
    let (handler, seen) = recorder();
    client.subscribe(0, handler).expect("subscribes");
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert!(seen.lock().expect("unpoisoned").is_empty());
}

#[test]
fn ack_resumes_from_the_durable_cursor() {
    // A reconnect with the same subscriber id resumes at max(from, cursor).
    let server = BusServer::start("resume");
    {
        let client = server.client();
        for _ in 0..3 {
            client
                .publish(incoming("agent.session.started", "s-1", json!({})))
                .expect("publishes");
        }
        let (handler, seen) = recorder();
        client.subscribe(0, handler).expect("subscribes");
        await_events(&seen, 3);
        client.ack(2).expect("acks");
    }

    let reconnected = server.client();
    let (handler, _seen) = recorder();
    let from_seq = reconnected.subscribe(0, handler).expect("subscribes");
    assert_eq!(from_seq, 2, "the resume never skips the acknowledged event");
}
