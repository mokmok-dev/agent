use agentd_events::{Event, EventLog, LogEntry, LogError, Seq, WireMessage};
use agentd_inference::{Delta, InferenceRequest, Provider};
use axum::Router;
use axum::extract::Query;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use futures_util::SinkExt;
use futures_util::stream::{SplitSink, SplitStream, StreamExt};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::broadcast::error::RecvError;
use tower_http::trace::TraceLayer;

use crate::auth::{Claim, TokenStore};

/// The largest number of historical events read from the log at once while
/// replaying to a client. Bounds the memory one replay step can hold.
const REPLAY_CHUNK: usize = 256;

/// The transient notice sent once a resume replay has caught up, so a client
/// can finish an interrupted turn from the replayed state.
const DAEMON_CAUGHT_UP: &str = "daemon.caught_up";

/// Errors returned by [`run`].
#[derive(Debug, Error)]
pub enum ServerError {
    /// Creating the socket, its parent directory, or the listener failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// A live instance is already listening on the socket.
    #[error("another instance is already listening on {0}")]
    AlreadyRunning(std::path::PathBuf),
    /// The socket's directory is writable by other users, so another local user
    /// could replace the socket and impersonate the daemon.
    #[error("the socket directory {0} is writable by other users")]
    InsecureDirectory(std::path::PathBuf),
}

/// Serves the HTTP and WebSocket API over the Unix domain socket at `socket`
/// until a SIGINT or SIGTERM is received.
///
/// If a socket file already exists at `socket`, it is probed before binding:
/// a connectable socket means another instance is live and
/// [`ServerError::AlreadyRunning`] is returned; otherwise the stale file is
/// removed and the socket is bound.
///
/// The socket's parent directory and the socket itself are made private to the
/// daemon user (mode `0700`/`0600`) when they are created, so another local user
/// cannot connect or inject events. Access is then decided by the bearer tokens
/// in `tokens` (see [`crate::auth`]).
///
/// # Errors
///
/// Returns [`ServerError::Io`] if creating the socket, its parent directory,
/// the listener, or the signal handlers fails, and
/// [`ServerError::AlreadyRunning`] if a live instance already owns `socket`.
pub async fn run(
    socket: std::path::PathBuf,
    log: EventLog,
    tokens: TokenStore,
    provider: Arc<dyn Provider>,
) -> Result<(), ServerError> {
    if let Some(parent) = socket.parent()
        && !parent.as_os_str().is_empty()
    {
        if parent.exists() {
            if is_shared_directory(parent) {
                return Err(ServerError::InsecureDirectory(parent.to_path_buf()));
            }
        } else {
            std::fs::create_dir_all(parent)?;
            set_mode(parent, 0o700)?;
        }
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
    set_mode(&socket, 0o600)?;

    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;

    let () = axum::serve(listener, router(log, tokens, provider))
        .with_graceful_shutdown(async move {
            tokio::select! {
                _ = interrupt.recv() => {},
                _ = terminate.recv() => {},
            }
        })
        .await?;

    Ok(())
}

/// Sets a filesystem mode on Unix; a no-op elsewhere.
#[cfg(unix)]
pub(crate) fn set_mode(
    path: &std::path::Path,
    mode: u32,
) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

/// Sets a filesystem mode on Unix; a no-op elsewhere.
#[cfg(not(unix))]
pub(crate) fn set_mode(
    _path: &std::path::Path,
    _mode: u32,
) -> std::io::Result<()> {
    Ok(())
}

/// Whether a directory is writable by users other than its owner.
///
/// A shared directory would let another local user unlink and replace the
/// socket, so the daemon refuses to serve from one.
#[cfg(unix)]
pub(crate) fn is_shared_directory(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path).is_ok_and(|metadata| metadata.permissions().mode() & 0o002 != 0)
}

/// Whether a directory is writable by users other than its owner.
#[cfg(not(unix))]
pub(crate) fn is_shared_directory(_path: &std::path::Path) -> bool {
    false
}

/// Builds the router exposing the event API and the inference endpoint.
pub fn router(
    log: EventLog,
    tokens: TokenStore,
    provider: Arc<dyn Provider>,
) -> Router {
    Router::new()
        .route("/events", any(events_handler))
        .route("/inference", any(inference_handler))
        .layer(TraceLayer::new_for_http())
        .with_state(AppState {
            log,
            tokens,
            provider,
        })
}

/// The state shared by the API handlers.
#[derive(Clone)]
struct AppState {
    /// The durable event log that inbound events are appended to.
    log: EventLog,
    /// The configured bearer tokens.
    tokens: TokenStore,
    /// The model provider inference is streamed from.
    provider: Arc<dyn Provider>,
}

impl std::fmt::Debug for AppState {
    /// Formats the state without the provider, which has no stable shape.
    fn fmt(
        &self,
        f: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("log", &self.log)
            .field("tokens", &self.tokens)
            .field("provider", &"<provider>")
            .finish()
    }
}

/// Query parameters of the event stream.
#[derive(Debug, Deserialize)]
struct Resume {
    /// The one-based log position to resume from, inclusive. When absent, only
    /// events appended after the connection are streamed (live-only).
    from: Option<Seq>,
}

/// Upgrades authenticated `GET /events` connections to bidirectional event
/// streams.
///
/// The `Authorization: Bearer` header selects a [`Principal`](crate::auth::Principal);
/// a missing or unknown token, or one that grants neither read nor publish, is
/// answered with `401 Unauthorized` before the upgrade. The optional `from`
/// query parameter resumes from a log position: history is replayed from `from`
/// and then the live stream continues without gaps or duplicates. Without it,
/// only events appended after the connection are sent.
async fn events_handler(
    State(AppState { log, tokens, .. }): State<AppState>,
    Query(resume): Query<Resume>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    let principal = match tokens.authorize(&headers) {
        Ok(principal) => principal,
        Err(error) => {
            tracing::debug!(%error, "rejected an unauthenticated event connection");
            return StatusCode::UNAUTHORIZED.into_response();
        },
    };
    upgrade.on_upgrade(move |socket| handle_events_socket(socket, log, principal, resume.from))
}

/// Upgrades authenticated `GET /inference` connections to a model stream.
///
/// The connection must hold [`Claim::Infer`]; a missing or unknown token is
/// answered with `401 Unauthorized`, a recognized token without the inference
/// claim with `403 Forbidden`. The volatile delta stream is not written to the
/// durable log.
async fn inference_handler(
    State(AppState {
        tokens, provider, ..
    }): State<AppState>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    let principal = match tokens.authorize(&headers) {
        Ok(principal) => principal,
        Err(error) => {
            tracing::debug!(%error, "rejected an unauthenticated inference connection");
            return StatusCode::UNAUTHORIZED.into_response();
        },
    };
    if !principal.has(Claim::Infer) {
        tracing::debug!("a token without the inference claim attempted to use /inference");
        return StatusCode::FORBIDDEN.into_response();
    }
    upgrade.on_upgrade(move |socket| handle_inference_socket(socket, provider))
}

/// Streams inference responses back to one client until it disconnects.
///
/// Each text frame is one [`InferenceRequest`]; the response is a sequence of
/// [`Delta`] text frames ending at the first terminal delta. A malformed
/// request or a provider failure is answered with a [`Delta::Error`] and the
/// connection stays open for the next request.
async fn handle_inference_socket(
    socket: WebSocket,
    provider: Arc<dyn Provider>,
) {
    let (mut sink, mut inbound) = socket.split();
    while let Some(message) = inbound.next().await {
        match message {
            Ok(Message::Text(text)) => {
                let request = match serde_json::from_str::<InferenceRequest>(&text) {
                    Ok(request) => request,
                    Err(error) => {
                        tracing::debug!(%error, "client sent an invalid inference request");
                        let delta = Delta::Error {
                            message: format!("invalid inference request: {error}"),
                        };
                        if send_delta(&mut sink, &delta).await.is_err() {
                            break;
                        }
                        continue;
                    },
                };
                if !stream_response(&mut sink, &mut inbound, provider.as_ref(), request).await {
                    break;
                }
            },
            Ok(Message::Close(_)) => break,
            Ok(_) => {},
            Err(error) => {
                tracing::debug!(%error, "inference connection failed");
                break;
            },
        }
    }
}

/// Streams one provider response to `sink`, returning `false` when the
/// connection should close.
///
/// The client's half of the socket is polled alongside the provider stream, so
/// a provider that stalls does not hold the task open after the client has
/// disconnected; frames arriving mid-stream are ignored, because the client
/// sends its next request only after a terminal delta.
async fn stream_response(
    sink: &mut SplitSink<WebSocket, Message>,
    inbound: &mut SplitStream<WebSocket>,
    provider: &dyn Provider,
    request: InferenceRequest,
) -> bool {
    let mut deltas = match provider.stream(request).await {
        Ok(deltas) => deltas,
        Err(error) => {
            let delta = Delta::Error {
                message: error.to_string(),
            };
            return send_delta(sink, &delta).await.is_ok();
        },
    };
    loop {
        tokio::select! {
            item = deltas.next() => {
                let Some(item) = item else {
                    return true;
                };
                let delta = item.unwrap_or_else(|error| Delta::Error {
                    message: error.to_string(),
                });
                let terminal = delta.is_terminal();
                if send_delta(sink, &delta).await.is_err() {
                    return false;
                }
                if terminal {
                    return true;
                }
            },
            message = inbound.next() => {
                match message {
                    Some(Ok(Message::Close(_))) | None => return false,
                    Some(Err(error)) => {
                        tracing::debug!(%error, "inference connection failed");
                        return false;
                    },
                    Some(Ok(_)) => {},
                }
            },
        }
    }
}

/// Sends one inference delta as a JSON text frame.
///
/// A delta that cannot be serialized is logged and skipped.
async fn send_delta(
    sink: &mut SplitSink<WebSocket, Message>,
    delta: &Delta,
) -> Result<(), axum::Error> {
    let Ok(text) = serde_json::to_string(delta) else {
        tracing::warn!("failed to serialize an inference delta");
        return Ok(());
    };
    sink.send(Message::Text(text.into())).await
}

/// Pumps events in both directions between the log and the socket until the
/// client disconnects or the log closes.
///
/// What the connection may do is decided by `principal`:
///
/// - [`Claim::Read`] enables the outbound stream (replay and live). Without it,
///   the socket is publish-only and receives nothing but notices.
/// - [`Claim::Publish`] enables inbound text frames. Without it, a publish
///   attempt is answered with an `error.unauthorized` notice.
///
/// Inbound text messages are parsed as [`Event`]s, validated, and durably
/// published with the daemon-owned `source` and `time` overwritten; an invalid
/// frame is answered with an `error.invalid_event` notice, a missing
/// [`Claim::Authority`] for a reserved type with `error.unauthorized`, and a
/// failed durable append with `error.publish_failed` instead of closing the
/// connection. Outbound messages are [`WireMessage`]s, pairing each event with
/// its log position (or `null` for a transient notice).
///
/// When `from` is `Some` and the connection can read, history is replayed from
/// that position before the live stream continues. A position outside
/// `1..=tail+1` (with `tail` the last committed position) is answered with an
/// `error.resume_out_of_range` notice and the connection is closed. A replay
/// failure also closes the connection after an `error.replay_failed` notice,
/// because continuing would leave a permanent gap. A subscriber that falls
/// behind while resuming recovers by re-reading the durable tail, so it never
/// silently skips events; without a resume position it is notified through an
/// `error.lagged` event, as before. Binary, ping, and pong frames are ignored;
/// pongs are answered automatically by the WebSocket implementation.
async fn handle_events_socket(
    socket: WebSocket,
    log: EventLog,
    principal: crate::auth::Principal,
    from: Option<Seq>,
) {
    let (mut sink, mut inbound) = socket.split();
    let can_read = principal.has(Claim::Read);

    if can_read && let Some(position) = from {
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

    if can_read
        && let Some(position) = from
        && let Err(error) = replay(&log, position, &mut sink, &mut last_sent).await
    {
        tracing::error!(%error, "failed to replay history");
        let notice = Event::new("error.replay_failed", json!({ "error": error.to_string() }));
        let _ = send_notice(&mut sink, notice).await;
        return;
    }

    if can_read && from.is_some() {
        let notice = Event::new(DAEMON_CAUGHT_UP, json!({ "tail": log.tail_seq() }));
        if send_notice(&mut sink, notice).await.is_err() {
            return;
        }
    }

    loop {
        tokio::select! {
            recorded = events.recv(), if can_read => {
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
                if !handle_inbound(&mut sink, &log, message, &principal).await {
                    break;
                }
            },
        }
    }
}

/// Handles one inbound frame, returning `false` when the connection should
/// close.
///
/// A text frame is published only when the connection holds
/// [`Claim::Publish`]; the event is validated, a reserved type additionally
/// requires [`Claim::Authority`], and the daemon overwrites `source` and `time`
/// before the durable append. Each refusal is answered with a notice rather
/// than closing the connection. Binary, ping, and pong frames are ignored.
async fn handle_inbound(
    sink: &mut SplitSink<WebSocket, Message>,
    log: &EventLog,
    message: Message,
    principal: &crate::auth::Principal,
) -> bool {
    match message {
        Message::Text(text) => {
            if !principal.has(Claim::Publish) {
                tracing::debug!("a read-only client attempted to publish");
                let reply = Event::new(
                    "error.unauthorized",
                    json!({ "error": "the token does not grant the publish claim" }),
                );
                return send_notice(sink, reply).await.is_ok();
            }
            let mut event = match serde_json::from_str::<Event>(&text) {
                Ok(event) => event,
                Err(error) => {
                    tracing::debug!(%error, "client sent an invalid event");
                    let reply =
                        Event::new("error.invalid_event", json!({ "error": error.to_string() }));
                    return send_notice(sink, reply).await.is_ok();
                },
            };
            if let Err(error) = event.validate() {
                tracing::debug!(%error, "client sent an event with invalid attributes");
                let reply =
                    Event::new("error.invalid_event", json!({ "error": error.to_string() }));
                return send_notice(sink, reply).await.is_ok();
            }
            if event.is_reserved() && !principal.has(Claim::Authority) {
                tracing::debug!(r#type = %event.r#type, "client attempted a reserved event type");
                let reply = Event::new(
                    "error.unauthorized",
                    json!({
                        "error": "the token does not grant the authority claim",
                        "type": event.r#type,
                    }),
                );
                return send_notice(sink, reply).await.is_ok();
            }
            event.set_provenance(principal.source());
            if let Err(error) = log.publish(event).await {
                tracing::error!(%error, "failed to durably publish an event");
                let reply = Event::new(
                    "error.publish_failed",
                    json!({ "error": error.to_string() }),
                );
                return send_notice(sink, reply).await.is_ok();
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
    use crate::auth::{Claim, Principal, Token, TokenStore};
    use agentd_events::{Event, EventLog, Seq, WireMessage};
    use agentd_inference::{Delta, FakeProvider, InferenceRequest, Provider};
    use futures_util::SinkExt;
    use futures_util::StreamExt;
    use serde_json::json;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::net::UnixStream;
    use tokio_tungstenite::WebSocketStream;
    use tokio_tungstenite::tungstenite::Message;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    /// A read-only token secret.
    const READ_TOKEN: &str = "read-secret";
    /// A read-and-publish token secret.
    const WRITE_TOKEN: &str = "write-secret";
    /// A read, publish, and authority token secret.
    const AUTHORITY_TOKEN: &str = "authority-secret";
    /// A read, publish, and inference token secret.
    const INFER_TOKEN: &str = "infer-secret";

    /// The token set used by the tests: read-only, read+publish, authority, and
    /// inference.
    fn tokens() -> TokenStore {
        TokenStore::new(vec![
            Token {
                secret: String::from(READ_TOKEN),
                principal: Principal::new("urn:test:reader", [Claim::Read]),
            },
            Token {
                secret: String::from(WRITE_TOKEN),
                principal: Principal::new("urn:test:writer", [Claim::Read, Claim::Publish]),
            },
            Token {
                secret: String::from(AUTHORITY_TOKEN),
                principal: Principal::new(
                    "urn:test:approver",
                    [Claim::Read, Claim::Publish, Claim::Authority],
                ),
            },
            Token {
                secret: String::from(INFER_TOKEN),
                principal: Principal::new(
                    "urn:test:agent",
                    [Claim::Read, Claim::Publish, Claim::Infer],
                ),
            },
        ])
    }

    /// A default fake provider for servers that do not exercise inference.
    fn provider() -> Arc<dyn Provider> {
        Arc::new(FakeProvider::default())
    }

    /// Opens a fresh log under `dir`.
    fn open_log(dir: &Path) -> EventLog {
        EventLog::open(dir.join("events.jsonl")).expect("log should open")
    }

    /// Spawns a server on `socket` without signal handling.
    fn spawn_server(
        socket: PathBuf,
        log: EventLog,
    ) -> tokio::task::JoinHandle<std::io::Result<()>> {
        spawn_server_with(socket, log, tokens(), provider())
    }

    /// Spawns a server with an explicit token store and provider.
    fn spawn_server_with(
        socket: PathBuf,
        log: EventLog,
        tokens: TokenStore,
        provider: Arc<dyn Provider>,
    ) -> tokio::task::JoinHandle<std::io::Result<()>> {
        tokio::spawn(async move {
            let listener = tokio::net::UnixListener::bind(socket)?;
            let () = axum::serve(listener, router(log, tokens, provider)).await?;
            Ok(())
        })
    }

    /// Connects a WebSocket client to `url` with `token`, retrying while the
    /// server starts up.
    async fn connect_with(
        url: &str,
        socket: &Path,
        token: &str,
    ) -> WebSocketStream<UnixStream> {
        for _ in 0..100 {
            if let Ok(stream) = UnixStream::connect(socket).await {
                let mut request = url.into_client_request().expect("client request");
                request.headers_mut().insert(
                    "authorization",
                    format!("Bearer {token}").parse().expect("header value"),
                );
                let (ws, _) = tokio_tungstenite::client_async(request, stream)
                    .await
                    .expect("handshake should succeed");
                return ws;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("could not connect to {socket:?}");
    }

    /// Connects a read-only client to `/events`.
    async fn connect(socket: &Path) -> WebSocketStream<UnixStream> {
        connect_with("ws://localhost/events", socket, READ_TOKEN).await
    }

    /// Connects a read-and-publish client to `/events`.
    async fn connect_writer(socket: &Path) -> WebSocketStream<UnixStream> {
        connect_with("ws://localhost/events", socket, WRITE_TOKEN).await
    }

    /// Connects a read-only client to `/events` resuming from `from`.
    async fn connect_from(
        socket: &Path,
        from: Seq,
    ) -> WebSocketStream<UnixStream> {
        connect_with(
            &format!("ws://localhost/events?from={from}"),
            socket,
            READ_TOKEN,
        )
        .await
    }

    /// Connects a read-and-publish client to `/events` resuming from `from`.
    async fn connect_from_writer(
        socket: &Path,
        from: Seq,
    ) -> WebSocketStream<UnixStream> {
        connect_with(
            &format!("ws://localhost/events?from={from}"),
            socket,
            WRITE_TOKEN,
        )
        .await
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

    /// Receives the next positioned event, skipping transient notices.
    async fn recv_event(client: &mut WebSocketStream<UnixStream>) -> (Seq, Event) {
        loop {
            let (seq, event) = recv_wire(client).await;
            if let Some(seq) = seq {
                return (seq, event);
            }
        }
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

    /// Asserts `received` is `sent` with the daemon-owned provenance overwritten.
    fn assert_attributed(
        received: &Event,
        sent: &Event,
        source: &str,
    ) {
        assert_eq!(received.id, sent.id);
        assert_eq!(received.r#type, sent.r#type);
        assert_eq!(received.data, sent.data);
        assert_eq!(received.source, source);
        assert!(received.time.is_some());
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

        let mut client = connect_writer(&socket).await;
        let event = test_event("test.event");

        send_event(&mut client, &event).await;

        let (seq, received) = recv_wire(&mut client).await;
        assert_eq!(seq, Some(1));
        assert_attributed(&received, &event, "urn:test:writer");

        server.abort();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn events_socket_relays_published_events_to_other_clients() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let socket = dir.path().join("test.sock");
        let server = spawn_server(socket.clone(), open_log(dir.path()));

        let mut subscriber = connect(&socket).await;
        let mut publisher = connect_writer(&socket).await;
        let event = test_event("test.event");

        send_event(&mut publisher, &event).await;

        let (seq, received) = recv_wire(&mut subscriber).await;
        assert_eq!(seq, Some(1));
        assert_attributed(&received, &event, "urn:test:writer");

        server.abort();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn events_socket_replies_with_error_event_for_invalid_messages() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let socket = dir.path().join("test.sock");
        let server = spawn_server(socket.clone(), open_log(dir.path()));

        let mut client = connect_writer(&socket).await;

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

        let mut client = connect_writer(&socket).await;

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

        let mut client = connect_writer(&socket).await;
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

        let (seq, event) = recv_event(&mut client).await;
        assert_eq!((seq, event.data["index"].as_u64()), (2, Some(1)));
        let (seq, event) = recv_event(&mut client).await;
        assert_eq!((seq, event.data["index"].as_u64()), (3, Some(2)));

        // The live stream continues after the replayed history.
        log.publish(numbered_event(3))
            .await
            .expect("publish should succeed");
        let (seq, event) = recv_event(&mut client).await;
        assert_eq!((seq, event.data["index"].as_u64()), (4, Some(3)));

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
        let mut client = connect_from_writer(&socket, 3).await;
        send_event(&mut client, &numbered_event(2)).await;

        let (seq, event) = recv_event(&mut client).await;
        assert_eq!(seq, 3);
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
            let (seq, _) = recv_event(&mut client).await;
            positions.push(seq);
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
            run(socket, open_log(dir.path()), tokens(), provider()).await,
            Err(ServerError::AlreadyRunning(_))
        ));

        Ok(())
    }

    #[tokio::test]
    async fn run_refuses_a_world_writable_socket_directory() -> Result<(), ServerError> {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir()?;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o777))?;
        let socket = dir.path().join("test.sock");

        assert!(matches!(
            run(socket, open_log(dir.path()), tokens(), provider()).await,
            Err(ServerError::InsecureDirectory(_))
        ));

        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unauthenticated_connections_are_rejected() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let socket = dir.path().join("test.sock");
        let server = spawn_server(socket.clone(), open_log(dir.path()));

        for _ in 0..100 {
            if let Ok(stream) = UnixStream::connect(&socket).await {
                let request = "ws://localhost/events"
                    .into_client_request()
                    .expect("client request");
                let result = tokio_tungstenite::client_async(request, stream).await;
                assert!(result.is_err(), "a tokenless connection must be refused");
                server.abort();
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("could not connect to {socket:?}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn read_only_connections_cannot_publish() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let socket = dir.path().join("test.sock");
        let log = open_log(dir.path());
        let server = spawn_server(socket.clone(), log.clone());

        let mut client = connect(&socket).await;
        send_event(&mut client, &test_event("test.event")).await;

        let (seq, event) = recv_wire(&mut client).await;
        assert_eq!(seq, None);
        assert_eq!(event.r#type, "error.unauthorized");
        assert_eq!(log.tail_seq(), 0, "nothing should have been appended");

        server.abort();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn reserved_types_require_authority() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let socket = dir.path().join("test.sock");
        let log = open_log(dir.path());
        let server = spawn_server(socket.clone(), log.clone());

        let mut client = connect_writer(&socket).await;
        send_event(&mut client, &test_event("sandbox.permission.granted")).await;

        let (seq, event) = recv_wire(&mut client).await;
        assert_eq!(seq, None);
        assert_eq!(event.r#type, "error.unauthorized");
        assert_eq!(event.data["type"], "sandbox.permission.granted");
        assert_eq!(log.tail_seq(), 0, "nothing should have been appended");

        server.abort();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn authority_can_publish_reserved_types() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let socket = dir.path().join("test.sock");
        let server = spawn_server(socket.clone(), open_log(dir.path()));

        let mut client = connect_with("ws://localhost/events", &socket, AUTHORITY_TOKEN).await;
        let event = test_event("sandbox.permission.granted");
        send_event(&mut client, &event).await;

        let (seq, received) = recv_wire(&mut client).await;
        assert_eq!(seq, Some(1));
        assert_attributed(&received, &event, "urn:test:approver");

        server.abort();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn daemon_overwrites_client_supplied_provenance() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let socket = dir.path().join("test.sock");
        let server = spawn_server(socket.clone(), open_log(dir.path()));

        let mut client = connect_writer(&socket).await;
        let mut event = test_event("test.event");
        event.source = String::from("urn:spoofed");
        event.time = Some(String::from("2000-01-01T00:00:00Z"));
        send_event(&mut client, &event).await;

        let (seq, received) = recv_wire(&mut client).await;
        assert_eq!(seq, Some(1));
        assert_eq!(received.source, "urn:test:writer");
        assert_ne!(received.time, event.time);

        server.abort();
    }

    /// Receives and decodes the next inference delta from `client`.
    async fn recv_delta(client: &mut WebSocketStream<UnixStream>) -> Delta {
        let received = tokio::time::timeout(Duration::from_secs(5), client.next())
            .await
            .expect("timed out waiting for a delta")
            .expect("stream should not end")
            .expect("read should succeed");
        let Message::Text(text) = received else {
            panic!("expected a text message, got {received:?}");
        };
        serde_json::from_str(&text).expect("expected a valid delta")
    }

    /// Sends `request` as an inference frame.
    async fn send_request(
        client: &mut WebSocketStream<UnixStream>,
        request: &InferenceRequest,
    ) {
        client
            .send(Message::from(
                serde_json::to_string(request).expect("request should serialize"),
            ))
            .await
            .expect("send should succeed");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn inference_requires_the_infer_claim() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let socket = dir.path().join("test.sock");
        let server = spawn_server(socket.clone(), open_log(dir.path()));

        for _ in 0..100 {
            if let Ok(stream) = UnixStream::connect(&socket).await {
                let mut request = "ws://localhost/inference"
                    .into_client_request()
                    .expect("client request");
                request.headers_mut().insert(
                    "authorization",
                    format!("Bearer {WRITE_TOKEN}")
                        .parse()
                        .expect("header value"),
                );
                let result = tokio_tungstenite::client_async(request, stream).await;
                assert!(
                    result.is_err(),
                    "a token without the infer claim must be refused"
                );
                server.abort();
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("could not connect to {socket:?}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn inference_streams_a_scripted_response() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let socket = dir.path().join("test.sock");
        let provider: Arc<dyn Provider> = Arc::new(FakeProvider::new(vec![vec![
            Delta::Text {
                text: String::from("hello "),
            },
            Delta::Text {
                text: String::from("world"),
            },
            Delta::Done {
                finish_reason: Some(String::from("stop")),
            },
        ]]));
        let server = spawn_server_with(socket.clone(), open_log(dir.path()), tokens(), provider);

        let mut client = connect_with("ws://localhost/inference", &socket, INFER_TOKEN).await;
        send_request(
            &mut client,
            &InferenceRequest {
                messages: vec![agentd_inference::Message::user("hi")],
                ..InferenceRequest::default()
            },
        )
        .await;

        assert_eq!(
            recv_delta(&mut client).await,
            Delta::Text {
                text: String::from("hello ")
            }
        );
        assert_eq!(
            recv_delta(&mut client).await,
            Delta::Text {
                text: String::from("world")
            }
        );
        assert!(matches!(recv_delta(&mut client).await, Delta::Done { .. }));

        server.abort();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn inference_reports_a_provider_failure_as_an_error_delta() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let socket = dir.path().join("test.sock");
        let provider: Arc<dyn Provider> = Arc::new(FakeProvider::new(vec![vec![Delta::Error {
            message: String::from("boom"),
        }]]));
        let server = spawn_server_with(socket.clone(), open_log(dir.path()), tokens(), provider);

        let mut client = connect_with("ws://localhost/inference", &socket, INFER_TOKEN).await;
        send_request(&mut client, &InferenceRequest::default()).await;

        assert_eq!(
            recv_delta(&mut client).await,
            Delta::Error {
                message: String::from("boom")
            }
        );

        server.abort();
    }
}
