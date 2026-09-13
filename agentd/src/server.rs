use agentd_events::{Event, EventBus, SPEC_VERSION};
use axum::Router;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use axum::routing::any;
use futures_util::SinkExt;
use futures_util::stream::{SplitSink, StreamExt};
use serde_json::json;
use thiserror::Error;
use tokio::sync::broadcast::error::RecvError;
use tower_http::trace::TraceLayer;

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
    bus: EventBus,
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

    let () = axum::serve(listener, router(bus))
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
pub fn router(bus: EventBus) -> Router {
    Router::new()
        .route("/events", any(events_handler))
        .layer(TraceLayer::new_for_http())
        .with_state(bus)
}

/// Upgrades `GET /events` connections to bidirectional event streams.
async fn events_handler(
    State(bus): State<EventBus>,
    upgrade: WebSocketUpgrade,
) -> Response {
    upgrade.on_upgrade(move |socket| handle_events_socket(socket, bus))
}

/// Pumps events in both directions between the bus and the socket until the
/// client disconnects or the bus closes.
///
/// Inbound text messages are parsed as [`Event`]s and published to the bus;
/// messages parsing successfully but announcing an unsupported `CloudEvents`
/// `specversion` are rejected like invalid messages. Outbound events are
/// forwarded as JSON text messages. Invalid messages are answered with an
/// `error.invalid_event` event instead of closing the connection, and a
/// subscriber that falls behind is notified through an `error.lagged` event.
/// Binary, ping, and pong frames are ignored; pongs are answered automatically
/// by the WebSocket implementation.
async fn handle_events_socket(
    socket: WebSocket,
    bus: EventBus,
) {
    let (mut sink, mut inbound) = socket.split();
    let mut events = bus.subscribe();

    loop {
        tokio::select! {
            event = events.recv() => {
                match event {
                    Ok(event) => {
                        if send_event(&mut sink, &event).await.is_err() {
                            break;
                        }
                    },
                    Err(RecvError::Lagged(missed)) => {
                        tracing::warn!(missed, "client fell behind the event bus");
                        let notice = Event::new("error.lagged", json!({ "missed": missed }));
                        if send_event(&mut sink, &notice).await.is_err() {
                            break;
                        }
                    },
                    Err(RecvError::Closed) => break,
                }
            },
            message = inbound.next() => {
                let Some(Ok(message)) = message else { break };
                match message {
                    Message::Text(text) => match serde_json::from_str::<Event>(&text) {
                        Ok(event) if event.specversion == SPEC_VERSION => bus.publish(event),
                        Ok(_) => {
                            tracing::debug!("client sent an event with an unsupported specversion");
                            let reply = Event::new(
                                "error.invalid_event",
                                json!({ "error": format!("specversion must be {SPEC_VERSION}") }),
                            );
                            if send_event(&mut sink, &reply).await.is_err() {
                                break;
                            }
                        },
                        Err(error) => {
                            tracing::debug!(%error, "client sent an invalid event");
                            let reply =
                                Event::new("error.invalid_event", json!({ "error": error.to_string() }));
                            if send_event(&mut sink, &reply).await.is_err() {
                                break;
                            }
                        },
                    },
                    Message::Close(_) => {
                        sink.close().await.unwrap_or_else(|error| {
                            tracing::debug!(%error, "failed to acknowledge client close");
                        });
                        break;
                    },
                    Message::Binary(_) | Message::Ping(_) | Message::Pong(_) => {},
                }
            },
        }
    }
}

/// Sends `event` to the socket as a JSON text message.
///
/// Events that cannot be serialized are logged and skipped.
async fn send_event(
    sink: &mut SplitSink<WebSocket, Message>,
    event: &Event,
) -> Result<(), axum::Error> {
    let Ok(text) = serde_json::to_string(event) else {
        tracing::warn!(kind = %event.kind, "failed to serialize event");
        return Ok(());
    };
    sink.send(Message::Text(text.into())).await
}

#[cfg(test)]
mod tests {
    use super::{ServerError, router, run};
    use agentd_events::{Event, EventBus};
    use futures_util::SinkExt;
    use futures_util::StreamExt;
    use serde_json::json;
    use std::path::{Path, PathBuf};
    use std::time::Duration;
    use tokio::net::UnixStream;
    use tokio_tungstenite::WebSocketStream;
    use tokio_tungstenite::tungstenite::Message;

    /// Spawns a server on `socket` without signal handling.
    fn spawn_server(
        socket: PathBuf,
        bus: EventBus,
    ) -> tokio::task::JoinHandle<std::io::Result<()>> {
        tokio::spawn(async move {
            let listener = tokio::net::UnixListener::bind(socket)?;
            let () = axum::serve(listener, router(bus)).await?;
            Ok(())
        })
    }

    /// Connects a WebSocket client to `/events`, retrying while the server
    /// starts up.
    async fn connect(socket: &Path) -> WebSocketStream<UnixStream> {
        for _ in 0..100 {
            if let Ok(stream) = UnixStream::connect(socket).await {
                let (ws, _) = tokio_tungstenite::client_async("ws://localhost/events", stream)
                    .await
                    .expect("handshake should succeed");
                return ws;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("could not connect to {socket:?}");
    }

    /// Decodes a text message into an event.
    fn decode_event(message: Message) -> Event {
        let Message::Text(text) = message else {
            panic!("expected a text message, got {message:?}");
        };
        serde_json::from_str(&text).expect("expected a valid event")
    }

    fn test_event(kind: &str) -> Event {
        Event::new(kind, json!({ "value": 1 }))
    }

    #[tokio::test(flavor = "current_thread")]
    async fn events_socket_publishes_inbound_messages_and_relays_them_back() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let socket = dir.path().join("test.sock");
        let server = spawn_server(socket.clone(), EventBus::new(16));

        let mut client = connect(&socket).await;
        let event = test_event("test.event");

        client
            .send(Message::from(
                serde_json::to_string(&event).expect("event should serialize"),
            ))
            .await
            .expect("send should succeed");

        let received = tokio::time::timeout(Duration::from_secs(5), client.next())
            .await
            .expect("timed out waiting for event")
            .expect("stream should not end")
            .expect("read should succeed");

        assert_eq!(decode_event(received), event);

        server.abort();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn events_socket_relays_published_events_to_other_clients() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let socket = dir.path().join("test.sock");
        let server = spawn_server(socket.clone(), EventBus::new(16));

        let mut subscriber = connect(&socket).await;
        let mut publisher = connect(&socket).await;
        let event = test_event("test.event");

        publisher
            .send(Message::from(
                serde_json::to_string(&event).expect("event should serialize"),
            ))
            .await
            .expect("send should succeed");

        let received = tokio::time::timeout(Duration::from_secs(5), subscriber.next())
            .await
            .expect("timed out waiting for event")
            .expect("stream should not end")
            .expect("read should succeed");

        assert_eq!(decode_event(received), event);

        server.abort();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn events_socket_replies_with_error_event_for_invalid_messages() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let socket = dir.path().join("test.sock");
        let server = spawn_server(socket.clone(), EventBus::new(16));

        let mut client = connect(&socket).await;

        client
            .send(Message::from("not an event"))
            .await
            .expect("send should succeed");

        let received = tokio::time::timeout(Duration::from_secs(5), client.next())
            .await
            .expect("timed out waiting for error event")
            .expect("stream should not end")
            .expect("read should succeed");

        assert_eq!(decode_event(received).kind, "error.invalid_event");

        server.abort();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn events_socket_replies_with_error_event_for_unsupported_specversions() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let socket = dir.path().join("test.sock");
        let server = spawn_server(socket.clone(), EventBus::new(16));

        let mut client = connect(&socket).await;

        client
            .send(Message::from(
                r#"{"id":"0199b7ea-8f4a-7d12-9c3a-2f8b1e4d6a90","source":"urn:test","specversion":"0.3","type":"test.event","data":{}}"#,
            ))
            .await
            .expect("send should succeed");

        let received = tokio::time::timeout(Duration::from_secs(5), client.next())
            .await
            .expect("timed out waiting for error event")
            .expect("stream should not end")
            .expect("read should succeed");

        let event = decode_event(received);
        assert_eq!(event.kind, "error.invalid_event");
        assert_eq!(event.data["error"], "specversion must be 1.0");

        server.abort();
    }

    #[tokio::test]
    async fn run_reports_already_running_when_socket_is_live() -> Result<(), ServerError> {
        let dir = tempfile::tempdir()?;
        let socket = dir.path().join("test.sock");
        let _listener = tokio::net::UnixListener::bind(&socket)?;

        assert!(matches!(
            run(socket, EventBus::new(1)).await,
            Err(ServerError::AlreadyRunning(_))
        ));

        Ok(())
    }
}
