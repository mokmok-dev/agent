//! End-to-end test for the daemon's egress `Publisher` bridge.
//!
//! The bridge is the daemon's part of the bus client: the sandbox authors an
//! event, the bridge maps it to the producer half of an envelope and publishes it
//! over the client. The generic client is covered in `agent/tests/client.rs`.
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
use agent::cloudevent::Event as CloudEvent;
use agent::server::Server;
use agent::transport::{Allowlist, Listener, WebSocketConfig};
use daemon::bus::{BusClient, BusPublisher, Config, Handler};
use sandbox::egress::Publisher;
use sandbox::events::Event;
use sandbox::policy::HostPort;
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
            "daemon-bridge-{tag}-{}-{unique}",
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

    fn client(&self) -> BusClient {
        BusClient::connect(&Config {
            socket: self.socket.clone(),
            subscriber_id: "bridge-test".to_owned(),
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

/// A handler that records every event it receives.
fn recorder() -> (Handler, Arc<Mutex<Vec<CloudEvent>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let handler = Box::new(move |event: &CloudEvent| {
        sink.lock().expect("unpoisoned").push(event.clone());
    });
    (handler, seen)
}

/// Poll `seen` until it holds at least `count` events, or fail.
fn await_events(
    seen: &Arc<Mutex<Vec<CloudEvent>>>,
    count: usize,
) -> Vec<CloudEvent> {
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
fn the_bridge_mints_an_id_and_publishes_a_request() {
    // The sandbox's `Publisher` seam over the daemon's client: it must mint a
    // unique id and publish an `egress.requested` the bus accepts.
    let server = BusServer::start("bridge");
    let client = Arc::new(server.client());
    let publisher = BusPublisher::new(Arc::clone(&client));

    let first = publisher.next_request_id();
    let second = publisher.next_request_id();
    assert_ne!(first, second, "each request id is unique");

    let (handler, seen) = recorder();
    client.subscribe(0, handler).expect("subscribes");

    let destination = HostPort::new("api.example.com", 443);
    publisher.publish(Event::egress_requested(
        "s-1",
        &first,
        &destination,
        Some("00-trace-span-01".to_owned()),
    ));

    let events = await_events(&seen, 1);
    let event = &events[0];
    assert_eq!(event.ty, sandbox::events::EGRESS_REQUESTED);
    assert_eq!(event.subject.as_deref(), Some("s-1"));
    assert_eq!(
        event.data.as_ref().expect("data")["request_id"],
        first.as_str()
    );
    assert_eq!(
        event.data.as_ref().expect("data")["host"],
        "api.example.com"
    );
    assert_eq!(event.data.as_ref().expect("data")["port"], 443);
    assert_eq!(
        event.extensions.get("traceparent"),
        Some(&json!("00-trace-span-01")),
        "the trace parent survives the bridge"
    );
}
