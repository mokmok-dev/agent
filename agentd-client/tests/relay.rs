//! End-to-end coverage of the client server: a real daemon router on a Unix
//! socket, the client server on a loopback TCP port, and WebSocket clients that
//! subscribe, publish, and answer a decision.
//!
//! The helpers use `expect` and `panic` like the other integration tests; the
//! workspace clippy configuration does not see integration test files, so the
//! allowances are replicated here.

#![expect(clippy::expect_used, clippy::panic)]

use agentd::auth::{Claim, Principal, Token, TokenStore};
use agentd::server;
use agentd_client::{Config, bind, serve};
use agentd_events::{Event, EventLog};
use agentd_inference::FakeProvider;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::net::{TcpStream, UnixListener};
use tokio::sync::watch;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderValue, header};

/// The token a read-and-publish client presents to the daemon.
const USER_TOKEN: &str = "user-secret";

/// The token an approver presents to the daemon.
const ADMIN_TOKEN: &str = "admin-secret";

/// The `CloudEvents` source the daemon records for the approver's token.
const ADMIN_SOURCE: &str = "urn:test:admin";

/// A bounded wait, so a broken assertion fails the test rather than hanging it.
const TIMEOUT: Duration = Duration::from_secs(5);

/// A daemon on a Unix socket and the client server on loopback.
struct Fixture {
    /// The durable log, read back directly by the tests.
    log_path: PathBuf,
    /// The client server's bound TCP address.
    address: SocketAddr,
    /// Signals the client server to stop.
    shutdown: watch::Sender<bool>,
    /// Keeps the temporary directory alive for the fixture's lifetime.
    _dir: tempfile::TempDir,
}

impl Fixture {
    /// Starts a daemon and a client server under it.
    async fn start(allow_approve: bool) -> Self {
        Self::start_with(allow_approve, Vec::new()).await
    }

    /// Starts a daemon and a client server that allows the given origins.
    async fn start_with(
        allow_approve: bool,
        allow_origins: Vec<String>,
    ) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("agentd.sock");
        let log_path = dir.path().join("events.jsonl");
        let log = EventLog::open(&log_path).expect("the log opens");
        let tokens = TokenStore::new(vec![
            Token {
                secret: USER_TOKEN.into(),
                principal: Principal::new("urn:test:user", [Claim::Read, Claim::Publish]),
            },
            Token {
                secret: ADMIN_TOKEN.into(),
                principal: Principal::new(
                    ADMIN_SOURCE,
                    [Claim::Read, Claim::Publish, Claim::Authority],
                ),
            },
        ]);
        let listener = UnixListener::bind(&socket).expect("the daemon binds");
        let router = server::router(log, tokens, Arc::new(FakeProvider::default()));
        tokio::spawn(async move {
            axum::serve(listener, router)
                .await
                .expect("the daemon serves");
        });

        let user_token_file = dir.path().join("user.token");
        std::fs::write(&user_token_file, USER_TOKEN).expect("the user token");
        let admin_token_file = dir.path().join("admin.token");
        std::fs::write(&admin_token_file, ADMIN_TOKEN).expect("the admin token");
        let config = Config {
            bind: "127.0.0.1:0".parse().expect("a loopback address"),
            socket: socket.clone(),
            token_file: user_token_file,
            admin_token_file: allow_approve.then_some(admin_token_file),
            allow_approve,
            publish_timeout: TIMEOUT,
            allow_origins,
        };
        let listener = bind(config.bind).await.expect("the client server binds");
        let address = listener.local_addr().expect("the bound address");
        let (shutdown, receiver) = watch::channel(false);
        tokio::spawn(async move {
            serve(listener, &config, receiver)
                .await
                .expect("the client server serves");
        });

        Self {
            log_path,
            address,
            shutdown,
            _dir: dir,
        }
    }

    /// Connects a downstream client, resuming from `from` when given.
    async fn connect(
        &self,
        from: Option<u64>,
    ) -> WebSocketStream<TcpStream> {
        let url = from.map_or_else(
            || format!("ws://{}/events", self.address),
            |from| format!("ws://{}/events?from={from}", self.address),
        );
        connect_ws(&url, self.address).await
    }

    /// The events durable in the log, in order.
    fn log_events(&self) -> Vec<Event> {
        let Ok(body) = std::fs::read_to_string(&self.log_path) else {
            return Vec::new();
        };
        body.lines()
            .filter_map(|line| serde_json::from_str::<Event>(line).ok())
            .collect()
    }
}

/// Connects a downstream WebSocket to `url` at `address`, retrying while the
/// client server starts.
async fn connect_ws(
    url: &str,
    address: SocketAddr,
) -> WebSocketStream<TcpStream> {
    for _ in 0..100 {
        let Ok(stream) = TcpStream::connect(address).await else {
            tokio::time::sleep(Duration::from_millis(20)).await;
            continue;
        };
        let request = url.into_client_request().expect("a request");
        let (socket, _) = tokio_tungstenite::client_async(request, stream)
            .await
            .expect("the handshake succeeds");
        return socket;
    }
    panic!("the client server did not accept a connection");
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.shutdown.send(true).ok();
    }
}

/// An event with a client-chosen id, as a downstream client would send it.
fn event(
    id: &str,
    r#type: &str,
    data: Value,
) -> Event {
    let mut event = Event::new(r#type, data);
    id.clone_into(&mut event.id);
    event
}

/// Reads frames until one carries `r#type`, and returns its whole envelope.
///
/// Frames of other types are discarded, so this is only for a wait whose other
/// frames do not matter.
async fn wait_for(
    client: &mut WebSocketStream<TcpStream>,
    r#type: &str,
) -> Value {
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let frame = tokio::time::timeout(remaining, client.next())
            .await
            .expect("a frame before the deadline")
            .expect("a frame")
            .expect("an ok frame");
        let Message::Text(text) = frame else {
            continue;
        };
        let wire: Value = serde_json::from_str(text.as_str()).expect("a wire envelope");
        if wire["event"]["type"].as_str() == Some(r#type) {
            return wire;
        }
    }
}

/// Reads frames until each of `wanted` has been seen once, and returns the first
/// envelope of each type.
///
/// Unlike [`wait_for`], a frame is never discarded, so the order in which the
/// daemon's echo and the client server's verdict arrive does not matter.
async fn collect(
    client: &mut WebSocketStream<TcpStream>,
    wanted: &[&'static str],
) -> HashMap<&'static str, Value> {
    let mut seen: HashMap<&'static str, Value> = HashMap::new();
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    while seen.len() < wanted.len() {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let frame = tokio::time::timeout(remaining, client.next())
            .await
            .expect("a frame before the deadline")
            .expect("a frame")
            .expect("an ok frame");
        let Message::Text(text) = frame else {
            continue;
        };
        let wire: Value = serde_json::from_str(text.as_str()).expect("a wire envelope");
        if let Some(found) = wire["event"]["type"]
            .as_str()
            .and_then(|found| wanted.iter().find(|wanted| **wanted == found))
        {
            seen.entry(found).or_insert(wire);
        }
    }
    seen
}

/// Sends one event as a downstream client's frame.
async fn send(
    client: &mut WebSocketStream<TcpStream>,
    event: &Event,
) {
    let text = serde_json::to_string(event).expect("the event serializes");
    client
        .send(Message::text(text))
        .await
        .expect("the frame is sent");
}

#[tokio::test]
async fn a_prompt_is_relayed_and_reported_committed() {
    let fixture = Fixture::start(false).await;
    let mut client = fixture.connect(None).await;
    let prompt = event(
        "018f6b2e-7e5c-7000-8000-000000000001",
        "agent.inbox",
        json!({ "conversation_id": "c1", "content": "hello" }),
    );

    send(&mut client, &prompt).await;
    let frames = collect(&mut client, &["client.publish_committed", "agent.inbox"]).await;
    let committed = &frames["client.publish_committed"];

    assert_eq!(
        committed["event"]["data"]["event_id"].as_str(),
        Some(prompt.id.as_str())
    );
    assert!(
        committed["event"]["data"]["seq"]
            .as_u64()
            .is_some_and(|seq| seq >= 1),
        "a committed publish carries its position: {committed}"
    );

    // The daemon's own frame also reaches the same connection, with a position.
    let echoed = &frames["agent.inbox"];
    assert_eq!(echoed["event"]["id"].as_str(), Some(prompt.id.as_str()));
    assert!(echoed["seq"].as_u64().is_some_and(|seq| seq >= 1));

    let recorded = fixture.log_events();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].r#type, "agent.inbox");
}

#[tokio::test]
async fn a_resume_replays_the_log_from_the_requested_position() {
    let fixture = Fixture::start(false).await;
    let prompt = event(
        "018f6b2e-7e5c-7000-8000-000000000002",
        "agent.inbox",
        json!({ "conversation_id": "c1", "content": "first" }),
    );
    {
        let mut client = fixture.connect(None).await;
        send(&mut client, &prompt).await;
        wait_for(&mut client, "client.publish_committed").await;
    }

    let mut resumed = fixture.connect(Some(1)).await;
    let replayed = wait_for(&mut resumed, "agent.inbox").await;
    assert_eq!(replayed["seq"].as_u64(), Some(1));
    assert_eq!(replayed["event"]["id"].as_str(), Some(prompt.id.as_str()));
}

#[tokio::test]
async fn a_reserved_type_is_refused_without_approvals() {
    let fixture = Fixture::start(false).await;
    let mut client = fixture.connect(None).await;
    let decision = event(
        "018f6b2e-7e5c-7000-8000-000000000003",
        "sandbox.permission.granted",
        json!({ "request_id": "req-1" }),
    );

    send(&mut client, &decision).await;
    let failed = wait_for(&mut client, "client.publish_failed").await;

    assert_eq!(
        failed["event"]["data"]["event_id"].as_str(),
        Some(decision.id.as_str())
    );
    let error = failed["event"]["data"]["error"]
        .as_str()
        .expect("the notice names the reason");
    assert!(error.contains("--allow-approve"), "{error}");
    assert!(
        fixture.log_events().is_empty(),
        "a refused event is never appended"
    );
}

#[tokio::test]
async fn a_decision_is_forwarded_with_approvals_enabled() {
    let fixture = Fixture::start(true).await;
    let mut client = fixture.connect(None).await;
    let decision = event(
        "018f6b2e-7e5c-7000-8000-000000000004",
        "sandbox.permission.granted",
        json!({ "request_id": "req-1" }),
    );

    send(&mut client, &decision).await;
    let committed = wait_for(&mut client, "client.publish_committed").await;
    assert_eq!(
        committed["event"]["data"]["event_id"].as_str(),
        Some(decision.id.as_str())
    );

    let recorded = fixture.log_events();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].r#type, "sandbox.permission.granted");
    assert_eq!(
        recorded[0].source, ADMIN_SOURCE,
        "the decision is published with the authority token"
    );
}

#[tokio::test]
async fn a_retried_id_is_not_appended_twice() {
    let fixture = Fixture::start(false).await;
    let mut client = fixture.connect(None).await;
    let prompt = event(
        "018f6b2e-7e5c-7000-8000-000000000005",
        "agent.inbox",
        json!({ "conversation_id": "c1", "content": "retry me" }),
    );

    send(&mut client, &prompt).await;
    let first = wait_for(&mut client, "client.publish_committed").await;
    send(&mut client, &prompt).await;
    let second = wait_for(&mut client, "client.publish_committed").await;

    assert_eq!(
        first["event"]["data"]["seq"],
        second["event"]["data"]["seq"]
    );
    let recorded = fixture.log_events();
    assert_eq!(
        recorded
            .iter()
            .filter(|event| event.id == prompt.id)
            .count(),
        1,
        "a retry from the verdict map never reaches the log"
    );
}

#[tokio::test]
async fn a_malformed_frame_is_answered_with_a_null_event_id() {
    let fixture = Fixture::start(false).await;
    let mut client = fixture.connect(None).await;

    client
        .send(Message::text("{ not json"))
        .await
        .expect("the frame is sent");
    let failed = wait_for(&mut client, "client.publish_failed").await;

    assert!(
        failed["event"]["data"]["event_id"].is_null(),
        "an unparsable frame has no id to correlate: {failed}"
    );
    let error = failed["event"]["data"]["error"]
        .as_str()
        .expect("the notice names the reason");
    assert!(error.contains("CloudEvent"), "{error}");
    assert!(fixture.log_events().is_empty());
}

#[tokio::test]
async fn shutdown_closes_an_open_connection() {
    let fixture = Fixture::start(false).await;
    let mut client = fixture.connect(None).await;

    fixture.shutdown.send(true).ok();
    let closed = tokio::time::timeout(TIMEOUT, async {
        while let Some(frame) = client.next().await {
            if !matches!(frame, Ok(Message::Text(_))) {
                break;
            }
        }
    })
    .await;

    assert!(
        closed.is_ok(),
        "an open connection must end itself on shutdown, or the graceful \
         shutdown never returns"
    );
}

#[tokio::test]
async fn an_absent_daemon_is_reported_as_upstream_lost() {
    let dir = tempfile::tempdir().expect("tempdir");
    let token_file = dir.path().join("user.token");
    std::fs::write(&token_file, USER_TOKEN).expect("the token");
    let config = Config {
        bind: "127.0.0.1:0".parse().expect("a loopback address"),
        socket: dir.path().join("absent.sock"),
        token_file,
        admin_token_file: None,
        allow_approve: false,
        publish_timeout: TIMEOUT,
        allow_origins: Vec::new(),
    };
    let listener = bind(config.bind).await.expect("the client server binds");
    let address = listener.local_addr().expect("the bound address");
    let (shutdown, receiver) = watch::channel(false);
    tokio::spawn(async move {
        serve(listener, &config, receiver)
            .await
            .expect("the client server serves");
    });

    let url = format!("ws://{address}/events");
    let mut client = connect_ws(&url, address).await;
    let lost = wait_for(&mut client, "client.upstream_lost").await;

    assert!(
        lost["event"]["data"]["error"].as_str().is_some(),
        "the notice names why the daemon could not be reached: {lost}"
    );
    shutdown.send(true).ok();
}

#[tokio::test]
async fn an_unanswered_publish_is_unknown_and_is_retried() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("agentd.sock");
    let token_file = dir.path().join("user.token");
    std::fs::write(&token_file, USER_TOKEN).expect("the token");
    // A stand-in daemon that completes the handshake and never answers, so the
    // read connection lives while a publish goes unresolved.
    let listener = UnixListener::bind(&socket).expect("the stand-in binds");
    let connects = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&connects);
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            counter.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                if let Ok(mut socket) = tokio_tungstenite::accept_async(stream).await {
                    while socket.next().await.is_some() {}
                }
            });
        }
    });

    let config = Config {
        bind: "127.0.0.1:0".parse().expect("a loopback address"),
        socket,
        token_file,
        admin_token_file: None,
        allow_approve: false,
        publish_timeout: Duration::from_millis(300),
        allow_origins: Vec::new(),
    };
    let listener = bind(config.bind).await.expect("the client server binds");
    let address = listener.local_addr().expect("the bound address");
    let (shutdown, receiver) = watch::channel(false);
    tokio::spawn(async move {
        serve(listener, &config, receiver)
            .await
            .expect("the client server serves");
    });
    let url = format!("ws://{address}/events");
    let mut client = connect_ws(&url, address).await;
    let prompt = event(
        "018f6b2e-7e5c-7000-8000-000000000006",
        "agent.inbox",
        json!({ "conversation_id": "c1", "content": "unanswered" }),
    );

    send(&mut client, &prompt).await;
    let failed = wait_for(&mut client, "client.publish_failed").await;
    assert_eq!(
        failed["event"]["data"]["outcome"].as_str(),
        Some("unknown"),
        "an unanswered publish is unresolved, not rejected: {failed}"
    );

    send(&mut client, &prompt).await;
    let retried = wait_for(&mut client, "client.publish_failed").await;
    assert_eq!(
        retried["event"]["data"]["outcome"].as_str(),
        Some("unknown")
    );
    assert!(
        connects.load(Ordering::SeqCst) >= 3,
        "an unresolved verdict is not remembered, so the retry opens a new \
         connection instead of being answered from the map"
    );
    shutdown.send(true).ok();
}

/// Tries one downstream connection carrying `origin`, and reports whether the
/// handshake succeeded.
async fn connect_with_origin(
    fixture: &Fixture,
    origin: &str,
) -> bool {
    let url = format!("ws://{}/events", fixture.address);
    let mut request = url.as_str().into_client_request().expect("a request");
    request.headers_mut().insert(
        header::ORIGIN,
        HeaderValue::from_str(origin).expect("a valid origin"),
    );
    let stream = TcpStream::connect(fixture.address)
        .await
        .expect("the connect");
    tokio_tungstenite::client_async(request, stream)
        .await
        .is_ok()
}

#[tokio::test]
async fn a_browser_origin_is_refused_unless_allowed() {
    // No origin is allow-listed by default, so a page cannot reach the socket.
    let closed = Fixture::start(false).await;
    assert!(!connect_with_origin(&closed, "https://evil.example").await);

    // A configured Web UI's origin is the one exception.
    let open = Fixture::start_with(false, vec![String::from("http://localhost:3000")]).await;
    assert!(!connect_with_origin(&open, "https://evil.example").await);
    assert!(connect_with_origin(&open, "http://localhost:3000").await);
}

#[tokio::test]
async fn a_locally_invalid_event_is_refused_with_its_id() {
    let fixture = Fixture::start(false).await;
    let mut client = fixture.connect(None).await;
    let invalid = event(
        "not-a-uuid",
        "agent.inbox",
        json!({ "conversation_id": "c1", "content": "x" }),
    );

    send(&mut client, &invalid).await;
    let failed = wait_for(&mut client, "client.publish_failed").await;

    assert_eq!(
        failed["event"]["data"]["event_id"].as_str(),
        Some("not-a-uuid"),
        "the refusal names the id the client chose: {failed}"
    );
    assert_eq!(
        failed["event"]["data"]["outcome"].as_str(),
        Some("rejected")
    );
    assert!(fixture.log_events().is_empty());
}

#[tokio::test]
async fn a_non_loopback_bind_is_refused() {
    let args = agentd_client::Args {
        socket: PathBuf::from("/tmp/agentd.sock"),
        token_file: PathBuf::from("/tmp/user.token"),
        admin_token_file: None,
        allow_approve: false,
        bind: "0.0.0.0:8787".parse().expect("an address"),
        allow_origin: Vec::new(),
        publish_timeout_secs: 10,
    };
    let error = Config::try_from(&args).expect_err("the command line must refuse it");
    assert!(matches!(error, agentd_client::ServerError::NotLoopback(_)));

    // The library path must refuse it too: an embedder never sees `Args`, and
    // the downstream socket carries no authentication.
    let error = bind("0.0.0.0:0".parse().expect("an address"))
        .await
        .expect_err("bind must refuse a non-loopback address");
    assert!(matches!(error, agentd_client::ServerError::NotLoopback(_)));
}
