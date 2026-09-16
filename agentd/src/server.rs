use agentd_events::{Event, EventLog, LogEntry, LogError, SPEC_VERSION, Seq, WireMessage};
use axum::Router;
use axum::extract::Query;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use axum::routing::any;
use futures_util::SinkExt;
use futures_util::stream::{SplitSink, StreamExt};
use serde::Deserialize;
use serde_json::json;
use thiserror::Error;
use tokio::sync::broadcast::error::RecvError;
use tower_http::trace::TraceLayer;

/// The largest number of historical events read from the log at once while
/// replaying to a client. Bounds the memory one replay step can hold.
const REPLAY_CHUNK: usize = 256;

/// Errors returned by [`run`].
#[derive(Debug, Error)]
pub enum ServerError {
    /// Creating the socket, its parent directory, or the listener failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// A live instance is already listening on the socket.
    #[error("another instance is already listening on {0}")]
    AlreadyRunning(std::path::PathBuf),
}

/// Serves the HTTP and WebSocket API over the Unix domain socket at `socket`
/// until a SIGINT or SIGTERM is received.
///
/// If a socket file already exists at `socket`, it is probed before binding:
/// a connectable socket means another instance is live and
/// [`ServerError::AlreadyRunning`] is returned; otherwise the stale file is
/// removed and the socket is bound.
///
/// # Errors
///
/// Returns [`ServerError::Io`] if creating the socket, its parent directory,
/// the listener, or the signal handlers fails, and
/// [`ServerError::AlreadyRunning`] if a live instance already owns `socket`.
pub async fn run(
    socket: std::path::PathBuf,
    log: EventLog,
) -> Result<(), ServerError> {
    if let Some(parent) = socket.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let listener = match tokio::net::UnixListener::bind(&socket) {
        Ok(listener) => listener,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            if tokio::net::UnixStream::connect(&socket).await.is_ok() {
                return Err(ServerError::AlreadyRunning(socket));
            }
            std::fs::remove_file(&socket)?;
            tokio::net::UnixListener::bind(&socket)?
        },
        Err(e) => return Err(e.into()),
    };

    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;

    let () = axum::serve(listener, router(log))
        .with_graceful_shutdown(async move {
            tokio::select! {
                _ = interrupt.recv() => {},
                _ = terminate.recv() => {},
            }
        })
        .await?;

    Ok(())
}

/// Builds the router exposing the event API.
pub fn router(log: EventLog) -> Router {
    Router::new()
        .route("/events", any(events_handler))
        .layer(TraceLayer::new_for_http())
        .with_state(log)
}

/// Query parameters of the event stream.
#[derive(Debug, Deserialize)]
struct Resume {
    /// The one-based log position to resume from, inclusive. When absent, only
    /// events appended after the connection are streamed (live-only).
    from: Option<Seq>,
}

/// Upgrades `GET /events` connections to bidirectional event streams.
///
/// The optional `from` query parameter resumes from a log position: history is
/// replayed from `from` and then the live stream continues without gaps or
/// duplicates. Without it, only events appended after the connection are sent.
async fn events_handler(
    State(log): State<EventLog>,
    Query(resume): Query<Resume>,
    upgrade: WebSocketUpgrade,
) -> Response {
    upgrade.on_upgrade(move |socket| handle_events_socket(socket, log, resume.from))
}

/// Pumps events in both directions between the log and the socket until the
/// client disconnects or the log closes.
///
/// Inbound text messages are parsed as [`Event`]s and durably published; a
/// message parsing successfully but announcing an unsupported `CloudEvents`
/// `specversion` is rejected like an invalid one. Outbound messages are
/// [`WireMessage`]s, pairing each event with its log position (or `null` for a
/// transient notice). Invalid messages are answered with an `error.invalid_event`
/// event and a failed durable append with an `error.publish_failed` event
/// instead of closing the connection.
///
/// When `from` is `Some`, history is replayed from that position before the live
/// stream continues. A position outside `1..=tail+1` (with `tail` the last
/// committed position) is answered with an `error.resume_out_of_range` notice
/// and the connection is closed. A replay failure also closes the connection
/// after an `error.replay_failed` notice, because continuing would leave a
/// permanent gap. A subscriber that falls behind while resuming recovers by
/// re-reading the durable tail, so it never silently skips events; without a
/// resume position it is notified through an `error.lagged` event, as before.
/// Binary, ping, and pong frames are ignored; pongs are answered automatically
/// by the WebSocket implementation.
async fn handle_events_socket(
    socket: WebSocket,
    log: EventLog,
    from: Option<Seq>,
) {
    let (mut sink, mut inbound) = socket.split();

    if let Some(position) = from {
        let tail = log.tail_seq();
        if position == 0 || position > tail.saturating_add(1) {
            tracing::debug!(position, tail, "resume position is out of range");
            let notice = Event::new(
                "error.resume_out_of_range",
                json!({ "position": position, "tail": tail }),
            );
            let _ = send_notice(&mut sink, notice).await;
            return;
        }
    }

    let mut events = log.subscribe();
    let resuming = from.is_some();
    let mut last_sent: Seq = from.map_or(0, |position| position.saturating_sub(1));

    if let Some(position) = from
        && let Err(error) = replay(&log, position, &mut sink, &mut last_sent).await
    {
        tracing::error!(%error, "failed to replay history");
        let notice = Event::new("error.replay_failed", json!({ "error": error.to_string() }));
        let _ = send_notice(&mut sink, notice).await;
        return;
    }

    loop {
        tokio::select! {
            recorded = events.recv() => {
                match recorded {
                    Ok(recorded) => {
                        if recorded.seq <= last_sent {
                            continue;
                        }
                        let position = recorded.seq;
                        if send_recorded(&mut sink, recorded).await.is_err() {
                            break;
                        }
                        last_sent = position;
                    },
                    Err(RecvError::Lagged(missed)) => {
                        tracing::warn!(missed, "client fell behind the event log");
                        if resuming {
                            let next = last_sent.saturating_add(1);
                            if let Err(error) = replay(&log, next, &mut sink, &mut last_sent).await {
                                tracing::error!(%error, "failed to replay missed events");
                                let notice = Event::new(
                                    "error.replay_failed",
                                    json!({ "error": error.to_string() }),
                                );
                                let _ = send_notice(&mut sink, notice).await;
                                break;
                            }
                        } else {
                            let notice = Event::new("error.lagged", json!({ "missed": missed }));
                            if send_notice(&mut sink, notice).await.is_err() {
                                break;
                            }
                        }
                    },
                    Err(RecvError::Closed) => break,
                }
            },
            message = inbound.next() => {
                let Some(Ok(message)) = message else { break };
                if !handle_inbound(&mut sink, &log, message).await {
                    break;
                }
            },
        }
    }
}

/// Handles one inbound frame, returning `false` when the connection should
/// close.
///
/// A text frame is parsed as an [`Event`] and durably published; an invalid
/// frame or an unsupported `specversion` is answered with an
/// `error.invalid_event` notice and a failed append with an
/// `error.publish_failed` notice, without closing the connection. Binary, ping,
/// and pong frames are ignored.
async fn handle_inbound(
    sink: &mut SplitSink<WebSocket, Message>,
    log: &EventLog,
    message: Message,
) -> bool {
    match message {
        Message::Text(text) => {
            match serde_json::from_str::<Event>(&text) {
                Ok(event) if event.specversion == SPEC_VERSION => {
                    if let Err(error) = log.publish(event).await {
                        tracing::error!(%error, "failed to durably publish an event");
                        let reply = Event::new(
                            "error.publish_failed",
                            json!({ "error": error.to_string() }),
                        );
                        return send_notice(sink, reply).await.is_ok();
                    }
                },
                Ok(_) => {
                    tracing::debug!("client sent an event with an unsupported specversion");
                    let reply = Event::new(
                        "error.invalid_event",
                        json!({ "error": format!("specversion must be {SPEC_VERSION}") }),
                    );
                    return send_notice(sink, reply).await.is_ok();
                },
                Err(error) => {
                    tracing::debug!(%error, "client sent an invalid event");
                    let reply =
                        Event::new("error.invalid_event", json!({ "error": error.to_string() }));
                    return send_notice(sink, reply).await.is_ok();
                },
            }
            true
        },
        Message::Close(_) => {
            sink.close().await.unwrap_or_else(|error| {
                tracing::debug!(%error, "failed to acknowledge client close");
            });
            false
        },
        Message::Binary(_) | Message::Ping(_) | Message::Pong(_) => true,
    }
}

/// Replays log entries from `from` to the end of the log, sending each to the
/// socket and advancing `last_sent`.
///
/// The blocking log reader runs on a worker thread in [`REPLAY_CHUNK`]-sized
/// batches, so a large replay does not stall the async runtime.
async fn replay(
    log: &EventLog,
    from: Seq,
    sink: &mut SplitSink<WebSocket, Message>,
    last_sent: &mut Seq,
) -> Result<(), ReplayError> {
    let mut reader = log.read_from(from)?;
    loop {
        let (returned, batch, done) = tokio::task::spawn_blocking(move || {
            let mut batch = Vec::with_capacity(REPLAY_CHUNK);
            let mut done = false;
            for _ in 0..REPLAY_CHUNK {
                if let Some(entry) = reader.next() {
                    batch.push(entry);
                } else {
                    done = true;
                    break;
                }
            }
            (reader, batch, done)
        })
        .await?;
        reader = returned;

        for entry in batch {
            let recorded = entry?;
            let position = recorded.seq;
            send_recorded(sink, recorded).await?;
            *last_sent = position.max(*last_sent);
        }

        if done {
            break;
        }
    }
    Ok(())
}

/// Errors returned while replaying history to a socket.
#[derive(Debug, Error)]
enum ReplayError {
    /// Opening or reading the log failed.
    #[error(transparent)]
    Log(#[from] LogError),
    /// The blocking replay task failed to join.
    #[error("the replay task failed: {0}")]
    Join(#[from] tokio::task::JoinError),
    /// Sending a replayed event to the socket failed.
    #[error(transparent)]
    Socket(#[from] axum::Error),
}

/// Sends `recorded` to the socket as a positioned JSON text message.
async fn send_recorded(
    sink: &mut SplitSink<WebSocket, Message>,
    recorded: LogEntry,
) -> Result<(), axum::Error> {
    send_message(sink, recorded.into()).await
}

/// Sends a transient daemon notice, which has no log position.
async fn send_notice(
    sink: &mut SplitSink<WebSocket, Message>,
    event: Event,
) -> Result<(), axum::Error> {
    send_message(sink, WireMessage::notice(event)).await
}

/// Sends `message` as a JSON text message.
///
/// Messages that cannot be serialized are logged and skipped.
async fn send_message(
    sink: &mut SplitSink<WebSocket, Message>,
    message: WireMessage,
) -> Result<(), axum::Error> {
    let Ok(text) = serde_json::to_string(&message) else {
        tracing::warn!(r#type = %message.event.r#type, "failed to serialize event");
        return Ok(());
    };
    sink.send(Message::Text(text.into())).await
}

#[cfg(test)]
mod tests {
    use super::{ServerError, router, run};
    use agentd_events::{Event, EventLog, Seq, WireMessage};
    use futures_util::SinkExt;
    use futures_util::StreamExt;
    use serde_json::json;
    use std::path::{Path, PathBuf};
    use std::time::Duration;
    use tokio::net::UnixStream;
    use tokio_tungstenite::WebSocketStream;
    use tokio_tungstenite::tungstenite::Message;

    /// Opens a fresh log under `dir`.
    fn open_log(dir: &Path) -> EventLog {
        EventLog::open(dir.join("events.jsonl")).expect("log should open")
    }

    /// Spawns a server on `socket` without signal handling.
    fn spawn_server(
        socket: PathBuf,
        log: EventLog,
    ) -> tokio::task::JoinHandle<std::io::Result<()>> {
        tokio::spawn(async move {
            let listener = tokio::net::UnixListener::bind(socket)?;
            let () = axum::serve(listener, router(log)).await?;
            Ok(())
        })
    }

    /// Connects a WebSocket client to `url`, retrying while the server starts
    /// up.
    async fn connect_to(
        url: &str,
        socket: &Path,
    ) -> WebSocketStream<UnixStream> {
        for _ in 0..100 {
            if let Ok(stream) = UnixStream::connect(socket).await {
                let (ws, _) = tokio_tungstenite::client_async(url, stream)
                    .await
                    .expect("handshake should succeed");
                return ws;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("could not connect to {socket:?}");
    }

    /// Connects a WebSocket client to `/events` without a resume position.
    async fn connect(socket: &Path) -> WebSocketStream<UnixStream> {
        connect_to("ws://localhost/events", socket).await
    }

    /// Connects a WebSocket client to `/events` resuming from `from`.
    async fn connect_from(
        socket: &Path,
        from: Seq,
    ) -> WebSocketStream<UnixStream> {
        connect_to(&format!("ws://localhost/events?from={from}"), socket).await
    }

    /// Decodes a text message into its position and event.
    fn decode_wire(message: Message) -> (Option<Seq>, Event) {
        let Message::Text(text) = message else {
            panic!("expected a text message, got {message:?}");
        };
        let wire: WireMessage = serde_json::from_str(&text).expect("expected a valid wire message");
        (wire.seq, wire.event)
    }

    /// Receives and decodes the next wire message from `client`.
    async fn recv_wire(client: &mut WebSocketStream<UnixStream>) -> (Option<Seq>, Event) {
        let received = tokio::time::timeout(Duration::from_secs(5), client.next())
            .await
            .expect("timed out waiting for a message")
            .expect("stream should not end")
            .expect("read should succeed");
        decode_wire(received)
    }

    /// Sends `event` as an inbound `CloudEvents` message.
    async fn send_event(
        client: &mut WebSocketStream<UnixStream>,
        event: &Event,
    ) {
        client
            .send(Message::from(
                serde_json::to_string(event).expect("event should serialize"),
            ))
            .await
            .expect("send should succeed");
    }

    fn test_event(r#type: &str) -> Event {
        Event::new(r#type, json!({ "value": 1 }))
    }

    fn numbered_event(index: u64) -> Event {
        Event::new("test.event", json!({ "index": index }))
    }

    #[tokio::test(flavor = "current_thread")]
    async fn events_socket_publishes_inbound_messages_and_relays_them_back() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let socket = dir.path().join("test.sock");
        let server = spawn_server(socket.clone(), open_log(dir.path()));

        let mut client = connect(&socket).await;
        let event = test_event("test.event");

        send_event(&mut client, &event).await;

        let (seq, received) = recv_wire(&mut client).await;
        assert_eq!(seq, Some(1));
        assert_eq!(received, event);

        server.abort();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn events_socket_relays_published_events_to_other_clients() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let socket = dir.path().join("test.sock");
        let server = spawn_server(socket.clone(), open_log(dir.path()));

        let mut subscriber = connect(&socket).await;
        let mut publisher = connect(&socket).await;
        let event = test_event("test.event");

        send_event(&mut publisher, &event).await;

        let (seq, received) = recv_wire(&mut subscriber).await;
        assert_eq!(seq, Some(1));
        assert_eq!(received, event);

        server.abort();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn events_socket_replies_with_error_event_for_invalid_messages() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let socket = dir.path().join("test.sock");
        let server = spawn_server(socket.clone(), open_log(dir.path()));

        let mut client = connect(&socket).await;

        client
            .send(Message::from("not an event"))
            .await
            .expect("send should succeed");

        let (seq, event) = recv_wire(&mut client).await;
        assert_eq!(seq, None);
        assert_eq!(event.r#type, "error.invalid_event");

        server.abort();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn events_socket_replies_with_error_event_for_unsupported_specversions() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let socket = dir.path().join("test.sock");
        let server = spawn_server(socket.clone(), open_log(dir.path()));

        let mut client = connect(&socket).await;

        client
            .send(Message::from(
                r#"{"id":"0199b7ea-8f4a-7d12-9c3a-2f8b1e4d6a90","source":"urn:test","specversion":"0.3","type":"test.event","data":{}}"#,
            ))
            .await
            .expect("send should succeed");

        let (seq, event) = recv_wire(&mut client).await;
        assert_eq!(seq, None);
        assert_eq!(event.r#type, "error.invalid_event");
        assert_eq!(event.data["error"], "specversion must be 1.0");

        server.abort();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn connecting_without_from_is_live_only() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let socket = dir.path().join("test.sock");
        let log = open_log(dir.path());
        let server = spawn_server(socket.clone(), log.clone());

        for index in 0..3 {
            log.publish(numbered_event(index))
                .await
                .expect("publish should succeed");
        }

        let mut client = connect(&socket).await;
        send_event(&mut client, &numbered_event(3)).await;

        // Only the newly published event arrives; the history is not replayed.
        let (seq, event) = recv_wire(&mut client).await;
        assert_eq!(seq, Some(4));
        assert_eq!(event.data["index"], 3);

        server.abort();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn events_socket_replays_history_from_a_position() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let socket = dir.path().join("test.sock");
        let log = open_log(dir.path());
        let server = spawn_server(socket.clone(), log.clone());

        for index in 0..3 {
            log.publish(numbered_event(index))
                .await
                .expect("publish should succeed");
        }

        let mut client = connect_from(&socket, 2).await;

        let (seq, event) = recv_wire(&mut client).await;
        assert_eq!((seq, event.data["index"].as_u64()), (Some(2), Some(1)));
        let (seq, event) = recv_wire(&mut client).await;
        assert_eq!((seq, event.data["index"].as_u64()), (Some(3), Some(2)));

        // The live stream continues after the replayed history.
        log.publish(numbered_event(3))
            .await
            .expect("publish should succeed");
        let (seq, event) = recv_wire(&mut client).await;
        assert_eq!((seq, event.data["index"].as_u64()), (Some(4), Some(3)));

        server.abort();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn resuming_at_the_tail_receives_only_live_events() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let socket = dir.path().join("test.sock");
        let log = open_log(dir.path());
        let server = spawn_server(socket.clone(), log.clone());

        for index in 0..2 {
            log.publish(numbered_event(index))
                .await
                .expect("publish should succeed");
        }

        // `tail` is 2, so 3 is the first position that has not been appended yet.
        let mut client = connect_from(&socket, 3).await;
        send_event(&mut client, &numbered_event(2)).await;

        let (seq, event) = recv_wire(&mut client).await;
        assert_eq!(seq, Some(3));
        assert_eq!(event.data["index"], 2);

        server.abort();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn resuming_beyond_the_tail_is_rejected() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let socket = dir.path().join("test.sock");
        let log = open_log(dir.path());
        let server = spawn_server(socket.clone(), log.clone());

        log.publish(numbered_event(0))
            .await
            .expect("publish should succeed");

        let mut client = connect_from(&socket, 3).await;

        let (seq, event) = recv_wire(&mut client).await;
        assert_eq!(seq, None);
        assert_eq!(event.r#type, "error.resume_out_of_range");
        assert_eq!(event.data["tail"], 1);

        server.abort();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn resuming_from_zero_is_rejected() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let socket = dir.path().join("test.sock");
        let server = spawn_server(socket.clone(), open_log(dir.path()));

        let mut client = connect_from(&socket, 0).await;

        let (seq, event) = recv_wire(&mut client).await;
        assert_eq!(seq, None);
        assert_eq!(event.r#type, "error.resume_out_of_range");

        server.abort();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn resuming_never_skips_or_repeats_positions() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let socket = dir.path().join("test.sock");
        let log = open_log(dir.path());
        let server = spawn_server(socket.clone(), log.clone());

        for index in 0..3 {
            log.publish(numbered_event(index))
                .await
                .expect("publish should succeed");
        }

        let mut client = connect_from(&socket, 1).await;

        // Publish concurrently with the replay so history and live overlap.
        let publisher = log.clone();
        let handle = tokio::spawn(async move {
            for index in 3..50 {
                publisher
                    .publish(numbered_event(index))
                    .await
                    .expect("publish should succeed");
            }
        });

        let mut positions = Vec::new();
        for _ in 0..50 {
            let (seq, _) = recv_wire(&mut client).await;
            positions.push(seq.expect("event should be positioned"));
        }
        assert_eq!(positions, (1..=50).collect::<Vec<Seq>>());

        handle.await.expect("publisher should finish");
        server.abort();
    }

    #[tokio::test]
    async fn run_reports_already_running_when_socket_is_live() -> Result<(), ServerError> {
        let dir = tempfile::tempdir()?;
        let socket = dir.path().join("test.sock");
        let _listener = tokio::net::UnixListener::bind(&socket)?;

        assert!(matches!(
            run(socket, open_log(dir.path())).await,
            Err(ServerError::AlreadyRunning(_))
        ));

        Ok(())
    }
}
