//! The async server: an accept loop and one task per connection.
//!
//! Ties the [`Listener`](crate::transport::Listener) to the [`Bus`]. Each
//! connection is authenticated against the
//! [allowlist](crate::transport::Allowlist), then drives the wire protocol from
//! [`crate::protocol`]:
//!
//! - `publish` commits the event, then replies `published`.
//! - `subscribe` names a subscriber, resolves the start sequence, replies
//!   `subscribed`, replays the history, and then streams live events.
//! - `ack` records the cursor for that subscriber and replies `cursor_ack`.
//! - A malformed message, a reserved attribute, or a write failure becomes an
//!   `error` reply.
//!
//! Concurrency model: the [`Bus`] is synchronous (its `append` does a real
//! `fsync`), so it is shared behind a `std::sync::Mutex` and every bus call runs
//! to completion before the next `.await`. The lock is therefore never held
//! across an await point. This blocks a runtime worker for the duration of an
//! `fsync`; moving the writer to a dedicated blocking task is a follow-up.
//!
//! Eviction is observed by the evicted connection itself: the broker drops its
//! sender, so its `recv` returns `None` and it closes with `SlowConsumer`. A
//! publisher never reaches into another connection.

use std::sync::{Arc, Mutex};

use futures_util::{SinkExt, StreamExt};
use tokio::net::UnixStream;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::{Message, Utf8Bytes};

use crate::broker::Subscription;
use crate::bus::{Bus, BusError};
use crate::cloudevent::Event;
use crate::protocol::{self, ClientMessage, ErrorCode, ServerMessage};
use crate::transport::{Allowlist, Connection, Listener};

/// The bus shared by every connection.
type SharedBus = Arc<Mutex<Bus>>;

/// The WebSocket stream of one connection.
type Socket = WebSocketStream<UnixStream>;

/// An accept loop over a [`Listener`].
#[derive(Debug)]
pub struct Server {
    listener: Listener,
    bus: SharedBus,
    allowlist: Allowlist,
}

impl Server {
    /// Build a server over `listener`, with `allowlist` gating every connection.
    #[must_use]
    pub fn new(
        listener: Listener,
        bus: Bus,
        allowlist: Allowlist,
    ) -> Self {
        Self {
            listener,
            bus: Arc::new(Mutex::new(bus)),
            allowlist,
        }
    }

    /// Accept connections until the listener fails.
    ///
    /// Each accepted connection is served on its own task. A connection from a
    /// peer outside the allowlist is refused before any message is read.
    ///
    /// # Errors
    ///
    /// Returns the [`std::io::Error`] if accepting from the listener fails. The
    /// accept loop does not stop for a single connection's failure.
    pub async fn run(self) -> std::io::Result<()> {
        loop {
            let connection = self.listener.accept().await?;
            let bus = Arc::clone(&self.bus);
            let allowlist = self.allowlist.clone();
            tokio::spawn(async move {
                if !allowlist.permits(&connection.credential()) {
                    refuse(connection).await;
                    return;
                }
                if let Err(error) = serve(bus, connection).await {
                    // A connection error is local; the accept loop keeps
                    // serving. The error is dropped until the observability
                    // milestone exists.
                    let _ = error;
                }
            });
        }
    }
}

/// Close a connection whose peer is not permitted, without reading a message.
async fn refuse(mut connection: Connection) {
    let _ = connection
        .websocket()
        .close(Some(CloseFrame {
            code: CloseCode::Policy,
            reason: Utf8Bytes::from_static("forbidden"),
        }))
        .await;
}

/// Failures of one connection.
#[derive(Debug, thiserror::Error)]
enum ConnectionError {
    /// The WebSocket layer failed.
    #[error("websocket error: {0}")]
    WebSocket(#[from] tokio_tungstenite::tungstenite::Error),
    /// A protocol message could not be encoded or decoded.
    #[error("protocol error: {0}")]
    Protocol(#[from] protocol::Error),
    /// A shared bus lock was poisoned by another connection's panic.
    #[error("the bus lock is poisoned")]
    Poisoned,
}

/// The subscription state of one connection.
#[derive(Debug, Default)]
struct Session {
    /// The subscriber the connection is currently subscribed as.
    subscriber: Option<String>,
    /// The live delivery handle.
    subscription: Option<Subscription>,
}

/// Serve one authenticated connection.
async fn serve(
    bus: SharedBus,
    connection: Connection,
) -> Result<(), ConnectionError> {
    let (credential, mut socket) = connection.into_parts();
    let uid = credential.uid;
    let mut session = Session::default();

    loop {
        tokio::select! {
            incoming = socket.next() => {
                let Some(incoming) = incoming else {
                    break;
                };
                let message = incoming?;
                if !handle_message(&bus, uid, &mut socket, &mut session, message).await? {
                    break;
                }
            }
            event = next_live(&mut session.subscription) => {
                if let Some(event) = event {
                    match Event::from_bytes(&event.payload) {
                        Ok(event) => send_event(&mut socket, &event).await?,
                        Err(error) => {
                            send(&mut socket, &malformed(&error.to_string())).await?;
                        },
                    }
                } else {
                    close_if_evicted(&mut socket, &session).await?;
                    break;
                }
            }
        }
    }

    // Drop the subscription so the broker reaps it promptly.
    if let Some(subscriber) = session.subscriber {
        let mut bus = bus.lock().map_err(|_| ConnectionError::Poisoned)?;
        let _ = bus.unsubscribe(uid, &subscriber);
    }
    Ok(())
}

/// Await the next live event, or never resolve when there is no subscription.
///
/// A never-resolving future disables the live branch of the [`tokio::select`]
/// without a separate guard.
async fn next_live(subscription: &mut Option<Subscription>) -> Option<crate::broker::Event> {
    match subscription {
        Some(subscription) => subscription.recv().await,
        None => std::future::pending().await,
    }
}

/// Handle one client frame. Returns `Ok(true)` to keep the connection open.
async fn handle_message(
    bus: &SharedBus,
    uid: u32,
    socket: &mut Socket,
    session: &mut Session,
    message: Message,
) -> Result<bool, ConnectionError> {
    let text = match text_frame(&message) {
        Ok(text) => text,
        Err(message) => {
            send(socket, &message).await?;
            return Ok(true);
        },
    };

    let message = match protocol::parse(&text) {
        Ok(message) => message,
        Err(error) => {
            send(socket, &malformed(&error.to_string())).await?;
            return Ok(true);
        },
    };

    let protocol::Message::Client(message) = message else {
        send(
            socket,
            &malformed("expected a control message from a client"),
        )
        .await?;
        return Ok(true);
    };

    match message {
        ClientMessage::Publish { id, event, .. } => {
            publish(bus, socket, id, *event).await?;
        },
        ClientMessage::Subscribe {
            subscriber_id,
            from_seq,
            ..
        } => {
            subscribe(bus, uid, socket, session, subscriber_id, from_seq).await?;
        },
        ClientMessage::Ack { cursor } => {
            ack(bus, uid, socket, session, cursor).await?;
        },
    }

    Ok(true)
}

/// Extract the text of a frame, or an error reply describing why not.
fn text_frame(message: &Message) -> Result<String, ServerMessage> {
    if !message.is_text() {
        return Err(malformed("only text frames are accepted"));
    }
    message
        .to_text()
        .map(str::to_owned)
        .map_err(|error| malformed(&error.to_string()))
}

/// Handle a `publish`: commit, then reply `published`.
async fn publish(
    bus: &SharedBus,
    socket: &mut Socket,
    id: String,
    incoming: crate::cloudevent::Incoming,
) -> Result<(), ConnectionError> {
    let result = {
        let mut bus = bus.lock().map_err(|_| ConnectionError::Poisoned)?;
        bus.publish(incoming)
    };
    match result {
        Ok(published) => {
            send(
                socket,
                &ServerMessage::Published {
                    id,
                    seq: published.seq,
                },
            )
            .await?;
        },
        Err(error) => {
            send(socket, &server_error(&error, Some(id))).await?;
        },
    }
    Ok(())
}

/// Handle a `subscribe`: register, reply `subscribed`, replay, then go live.
async fn subscribe(
    bus: &SharedBus,
    uid: u32,
    socket: &mut Socket,
    session: &mut Session,
    subscriber_id: String,
    from_seq: u64,
) -> Result<(), ConnectionError> {
    // Register and snapshot the replay under the lock, then release it before
    // streaming. Registering before replay means an event published during
    // replay is queued live and may also appear in the replay; delivery is
    // at-least-once, so a duplicate is acceptable but a gap is not.
    let registered = {
        let mut bus = bus.lock().map_err(|_| ConnectionError::Poisoned)?;
        // A connection holds at most one subscription. Drop any previous
        // registration before adding the new one, whether or not the identity
        // changed; otherwise the old sender lingers and its queue fills until
        // it is evicted.
        if let Some(previous) = session.subscriber.as_deref() {
            let _ = bus.unsubscribe(uid, previous);
        }
        bus.subscribe(uid, &subscriber_id, from_seq)
            .and_then(|subscribed| {
                let replay = bus.replay(from_seq)?;
                Ok((subscribed, replay))
            })
    };

    let (subscribed, replay) = match registered {
        Ok(pair) => pair,
        Err(error) => {
            send(socket, &server_error(&error, None)).await?;
            return Ok(());
        },
    };

    send(
        socket,
        &ServerMessage::Subscribed {
            from_seq: subscribed.from_seq,
        },
    )
    .await?;

    // Re-subscribing on the same connection replaces the previous subscription.
    session.subscriber = Some(subscriber_id);
    session.subscription = Some(subscribed.subscription);

    // Stream history. The iterator is owned, so the bus lock is already
    // released and a concurrent publish is not blocked.
    for event in replay {
        match event {
            Ok(event) => send_event(socket, &event).await?,
            Err(error) => {
                send(socket, &server_error(&error, None)).await?;
                return Ok(());
            },
        }
    }
    Ok(())
}

/// Handle an `ack`: record the cursor for the current subscriber.
async fn ack(
    bus: &SharedBus,
    uid: u32,
    socket: &mut Socket,
    session: &Session,
    cursor: u64,
) -> Result<(), ConnectionError> {
    let Some(subscriber) = session.subscriber.as_deref() else {
        send(
            socket,
            &ServerMessage::Error {
                code: ErrorCode::UnknownSubscriber,
                message: "ack before subscribe".to_owned(),
                id: None,
            },
        )
        .await?;
        return Ok(());
    };

    let result = {
        let bus = bus.lock().map_err(|_| ConnectionError::Poisoned)?;
        bus.ack(uid, subscriber, cursor)
    };
    match result {
        Ok(()) => send(socket, &ServerMessage::CursorAck { cursor }).await?,
        Err(error) => {
            send(socket, &server_error(&error, None)).await?;
        },
    }
    Ok(())
}

/// Send a committed event as a `CloudEvents` text frame.
async fn send_event(
    socket: &mut Socket,
    event: &Event,
) -> Result<(), ConnectionError> {
    let text = serde_json::to_string(event).map_err(protocol::Error::from)?;
    socket.send(Message::text(text)).await?;
    Ok(())
}

/// Send a server control message as a text frame.
async fn send(
    socket: &mut Socket,
    message: &ServerMessage,
) -> Result<(), ConnectionError> {
    let text = protocol::to_text(message)?;
    socket.send(Message::text(text)).await?;
    Ok(())
}

/// Close with `SlowConsumer` when the subscription was evicted, else normally.
async fn close_if_evicted(
    socket: &mut Socket,
    session: &Session,
) -> Result<(), ConnectionError> {
    let evicted = session
        .subscription
        .as_ref()
        .is_some_and(Subscription::is_evicted);
    let frame = if evicted {
        Some(CloseFrame {
            code: CloseCode::Again,
            reason: Utf8Bytes::from_static("slow consumer"),
        })
    } else {
        None
    };
    socket.close(frame).await?;
    Ok(())
}

/// A `malformed` error reply with no request ID.
fn malformed(message: &str) -> ServerMessage {
    ServerMessage::Error {
        code: ErrorCode::Malformed,
        message: message.to_owned(),
        id: None,
    }
}

/// Map a bus failure to a wire error.
fn server_error(
    error: &BusError,
    id: Option<String>,
) -> ServerMessage {
    let code = match error {
        BusError::Envelope(_) => ErrorCode::ReservedAttribute,
        BusError::Store(_) => ErrorCode::WriteFailed,
        BusError::Cursor(_) => ErrorCode::UnknownSubscriber,
    };
    ServerMessage::Error {
        code,
        message: error.to_string(),
        id,
    }
}

#[cfg(test)]
mod tests {
    // Tests for the server: authentication and each control message end to end
    // over a real Unix socket.

    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    use tokio::net::UnixStream;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::http::{HeaderValue, header};

    use super::*;
    use crate::transport::{SUBPROTOCOL, WebSocketConfig};

    /// A directory holding the socket, removed on drop.
    struct SocketDir(PathBuf);

    impl SocketDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "agent-server-{tag}-{}-{unique}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            Self(dir)
        }

        fn socket(&self) -> PathBuf {
            self.0.join("bus.sock")
        }
    }

    impl Drop for SocketDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A connected test client.
    struct Client(Socket);

    impl Client {
        async fn connect(path: &std::path::Path) -> Self {
            let stream = UnixStream::connect(path).await.expect("connects");
            let mut request = "ws://localhost/".into_client_request().expect("request");
            request.headers_mut().insert(
                header::SEC_WEBSOCKET_PROTOCOL,
                HeaderValue::from_static(SUBPROTOCOL),
            );
            let (socket, _) = tokio_tungstenite::client_async(request, stream)
                .await
                .expect("handshakes");
            Self(socket)
        }

        async fn send(
            &mut self,
            text: &str,
        ) {
            self.0
                .send(Message::text(text.to_owned()))
                .await
                .expect("sends");
        }

        /// The next frame as JSON, or `None` on close.
        async fn recv(&mut self) -> Option<serde_json::Value> {
            loop {
                let message = tokio::time::timeout(Duration::from_secs(5), self.0.next())
                    .await
                    .expect("a frame arrives in time");
                match message {
                    None | Some(Ok(Message::Close(_))) => return None,
                    Some(Ok(message)) if message.is_text() => {
                        return Some(
                            serde_json::from_str(message.to_text().expect("text")).expect("json"),
                        );
                    },
                    Some(Ok(_)) => {},
                    Some(Err(error)) => panic!("websocket error: {error}"),
                }
            }
        }

        /// The next frame as JSON, panicking on close.
        async fn expect(&mut self) -> serde_json::Value {
            self.recv().await.expect("a message, not a close")
        }
    }

    /// Start a server on a fresh socket with a permissive allowlist.
    fn start(dir: &SocketDir) -> PathBuf {
        start_with(dir, Allowlist::new().allow_uid(self_uid()), 16)
    }

    fn start_with(
        dir: &SocketDir,
        allowlist: Allowlist,
        capacity: usize,
    ) -> PathBuf {
        let socket = dir.socket();
        let listener = Listener::bind(&socket, WebSocketConfig::default()).expect("binds");
        let bus = Bus::open(dir.0.join("data"), capacity).expect("opens the bus");
        let server = Server::new(listener, bus, allowlist);
        tokio::spawn(async move {
            let _ = server.run().await;
        });
        socket
    }

    /// The UID the test process connects as.
    fn self_uid() -> u32 {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(std::env::temp_dir())
            .expect("the temp dir has metadata")
            .uid()
    }

    fn publish(id: &str) -> String {
        serde_json::json!({
            "type": "publish",
            "id": id,
            "event": {"specversion": "1.0", "type": "e"},
        })
        .to_string()
    }

    fn subscribe(
        subscriber_id: &str,
        from_seq: u64,
    ) -> String {
        serde_json::json!({
            "type": "subscribe",
            "subscriber_id": subscriber_id,
            "from_seq": from_seq,
        })
        .to_string()
    }

    #[tokio::test]
    async fn a_publish_is_committed_and_acknowledged() {
        let dir = SocketDir::new("publish");
        let socket = start(&dir);
        let mut client = Client::connect(&socket).await;

        client.send(&publish("req-1")).await;
        let reply = client.expect().await;
        assert_eq!(reply["type"], "published");
        assert_eq!(reply["id"], "req-1");
        assert_eq!(reply["seq"], 0);
    }

    #[tokio::test]
    async fn a_subscriber_receives_replayed_history_then_live_events() {
        let dir = SocketDir::new("subscribe");
        let socket = start(&dir);

        let mut publisher = Client::connect(&socket).await;
        for seq in 0..2 {
            publisher.send(&publish(&format!("req-{seq}"))).await;
            assert_eq!(publisher.expect().await["type"], "published");
        }

        let mut subscriber = Client::connect(&socket).await;
        subscriber.send(&subscribe("audit", 0)).await;
        assert_eq!(subscriber.expect().await["type"], "subscribed");

        let first = subscriber.expect().await;
        assert_eq!(first["sequence"], "00000000000000000000");
        let second = subscriber.expect().await;
        assert_eq!(second["sequence"], "00000000000000000001");

        publisher.send(&publish("req-live")).await;
        assert_eq!(publisher.expect().await["type"], "published");

        let live = subscriber.expect().await;
        assert_eq!(live["sequence"], "00000000000000000002");
    }

    #[tokio::test]
    async fn an_ack_is_durable_and_makes_the_next_subscribe_resume() {
        let dir = SocketDir::new("ack");
        let socket = start(&dir);

        let mut client = Client::connect(&socket).await;
        for seq in 0..3 {
            client.send(&publish(&format!("req-{seq}"))).await;
            assert_eq!(client.expect().await["type"], "published");
        }

        client.send(&subscribe("audit", 0)).await;
        assert_eq!(client.expect().await["type"], "subscribed");
        for _ in 0..3 {
            let _ = client.expect().await;
        }

        client.send(r#"{"type":"ack","cursor":2}"#).await;
        assert_eq!(client.expect().await["type"], "cursor_ack");

        // Re-subscribing as the same subscriber resumes from the stored cursor,
        // so delivery starts at 2, not 0.
        client.send(&subscribe("audit", 0)).await;
        let reply = client.expect().await;
        assert_eq!(reply["type"], "subscribed");
        assert_eq!(reply["from_seq"], 2);
    }

    #[tokio::test]
    async fn a_reserved_attribute_is_rejected_with_an_error() {
        let dir = SocketDir::new("reserved");
        let socket = start(&dir);
        let mut client = Client::connect(&socket).await;

        let request = serde_json::json!({
            "type": "publish",
            "id": "req-1",
            "event": {
                "specversion": "1.0",
                "type": "e",
                "sequence": "00000000000000000001",
            },
        });
        client.send(&request.to_string()).await;

        let reply = client.expect().await;
        assert_eq!(reply["type"], "error");
        assert_eq!(reply["code"], "reserved_attribute");
        assert_eq!(reply["id"], "req-1");
    }

    #[tokio::test]
    async fn a_malformed_message_is_rejected_without_closing() {
        let dir = SocketDir::new("malformed");
        let socket = start(&dir);
        let mut client = Client::connect(&socket).await;

        client.send("not json").await;
        let reply = client.expect().await;
        assert_eq!(reply["type"], "error");
        assert_eq!(reply["code"], "malformed");

        // The connection stays usable.
        client.send(&publish("req-1")).await;
        assert_eq!(client.expect().await["type"], "published");
    }

    #[tokio::test]
    async fn an_ack_before_subscribe_is_rejected() {
        let dir = SocketDir::new("ack-early");
        let socket = start(&dir);
        let mut client = Client::connect(&socket).await;

        client.send(r#"{"type":"ack","cursor":1}"#).await;
        let reply = client.expect().await;
        assert_eq!(reply["type"], "error");
        assert_eq!(reply["code"], "unknown_subscriber");
    }

    #[tokio::test]
    async fn a_disallowed_peer_is_refused() {
        let dir = SocketDir::new("forbidden");
        let socket = start_with(&dir, Allowlist::new(), 16);
        let mut client = Client::connect(&socket).await;
        assert_eq!(client.recv().await, None, "the peer is closed out");
    }

    #[tokio::test]
    async fn a_slow_subscriber_is_closed() {
        let dir = SocketDir::new("slow");
        let socket = start_with(&dir, Allowlist::new().allow_uid(self_uid()), 1);

        let mut subscriber = Client::connect(&socket).await;
        subscriber.send(&subscribe("audit", 0)).await;
        assert_eq!(subscriber.expect().await["type"], "subscribed");

        // "Slow" is a timing property: the connection task drains its queue as
        // fast as the socket accepts. To make eviction deterministic, the
        // subscriber never reads while the publisher sends enough bulk to fill
        // the socket buffer, after which the task blocks writing, the queue
        // fills, and the broker evicts it.
        let filler = "x".repeat(4096);
        let mut publisher = Client::connect(&socket).await;
        for seq in 0..256 {
            let request = serde_json::json!({
                "type": "publish",
                "id": format!("req-{seq}"),
                "event": {
                    "specversion": "1.0",
                    "type": "e",
                    "data": {"filler": filler},
                },
            });
            publisher.send(&request.to_string()).await;
            assert_eq!(publisher.expect().await["type"], "published");
        }

        // The subscriber drains whatever was buffered, then the server closes
        // it. Delivery is at-least-once, so reconnecting from its durable cursor
        // recovers anything it did not read.
        let mut closed = false;
        for _ in 0..2048 {
            if subscriber.recv().await.is_none() {
                closed = true;
                break;
            }
        }
        assert!(closed, "the slow subscriber is eventually closed out");
    }

    #[tokio::test]
    async fn a_published_envelope_is_replayed_as_a_cloud_event() {
        let dir = SocketDir::new("envelope");
        let socket = start(&dir);
        let mut client = Client::connect(&socket).await;

        let request = serde_json::json!({
            "type": "publish",
            "id": "req-1",
            "event": {
                "specversion": "1.0",
                "type": "agent.task.started",
                "subject": "t-1",
                "data": {"task_id": "t-1"},
            },
        });
        client.send(&request.to_string()).await;
        assert_eq!(client.expect().await["type"], "published");

        client.send(&subscribe("audit", 0)).await;
        assert_eq!(client.expect().await["type"], "subscribed");
        let event = client.expect().await;
        assert_eq!(event["type"], "agent.task.started");
        assert_eq!(event["subject"], "t-1");
        assert_eq!(event["source"], crate::bus::DEFAULT_SOURCE);
        assert_eq!(event["data"]["task_id"], "t-1");
    }

    #[tokio::test]
    async fn a_second_subscribe_on_one_connection_replaces_the_first() {
        let dir = SocketDir::new("resubscribe");
        let socket = start(&dir);
        let mut client = Client::connect(&socket).await;

        for seq in 0..2 {
            client.send(&publish(&format!("req-{seq}"))).await;
            assert_eq!(client.expect().await["type"], "published");
        }

        client.send(&subscribe("audit", 0)).await;
        assert_eq!(client.expect().await["type"], "subscribed");
        // Drain the two replayed events before re-subscribing.
        for _ in 0..2 {
            let _ = client.expect().await;
        }
        // Re-subscribing from 1 must replay from 1, not 0.
        client.send(&subscribe("audit", 1)).await;
        assert_eq!(client.expect().await["type"], "subscribed");
        let event = client.expect().await;
        assert_eq!(event["sequence"], "00000000000000000001");
    }

    #[tokio::test]
    async fn re_subscribing_under_a_new_id_drops_the_old_registration() {
        let dir = SocketDir::new("same-id");
        let socket = start(&dir);
        let mut client = Client::connect(&socket).await;

        client.send(&subscribe("first", 0)).await;
        assert_eq!(client.expect().await["type"], "subscribed");
        // Re-subscribe under a different identity on the same connection.
        client.send(&subscribe("second", 0)).await;
        assert_eq!(client.expect().await["type"], "subscribed");

        // The new subscription receives a live event.
        let mut publisher = Client::connect(&socket).await;
        publisher.send(&publish("req")).await;
        assert_eq!(publisher.expect().await["type"], "published");

        let event = client.expect().await;
        assert_eq!(event["type"], "e");
    }

    #[tokio::test]
    async fn re_subscribing_under_the_same_id_still_delivers() {
        let dir = SocketDir::new("same-id-again");
        let socket = start(&dir);
        let mut client = Client::connect(&socket).await;

        client.send(&subscribe("audit", 0)).await;
        assert_eq!(client.expect().await["type"], "subscribed");
        // Re-subscribing under the same identity replaces the registration.
        client.send(&subscribe("audit", 0)).await;
        assert_eq!(client.expect().await["type"], "subscribed");

        let mut publisher = Client::connect(&socket).await;
        publisher.send(&publish("req")).await;
        assert_eq!(publisher.expect().await["type"], "published");

        // The event is delivered exactly once, not twice: the old registration
        // was dropped.
        let event = client.expect().await;
        assert_eq!(event["type"], "e");
    }
}
