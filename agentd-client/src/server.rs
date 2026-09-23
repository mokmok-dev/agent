//! The downstream WebSocket server: one daemon connection per client, relaying
//! the daemon's own frames and forwarding the client's publishes.

use crate::error::RunError;
use crate::frame::{PUBLISH_FAILED, UPSTREAM_LOST, notice, wire};
use crate::policy::route;
use crate::uplink::PublishRequest;
use agentd_events::{Event, Seq};
use agentd_node::{ClientError, WsClient};
use axum::Router;
use axum::extract::Query;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use axum::routing::any;
use futures_util::SinkExt;
use futures_util::stream::{SplitSink, StreamExt};
use secrecy::{ExposeSecret as _, SecretString};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch};

/// The largest downstream frame accepted, matching the daemon's bridge cap.
const MAX_FRAME_BYTES: usize = 64 * 1024;

/// How many verdict notices may queue for one client before the publish task
/// backpressures.
const VERDICT_CAPACITY: usize = 16;

/// The delay before the first upstream connection retry; doubled each failure.
const INITIAL_BACKOFF: Duration = Duration::from_millis(100);

/// The longest delay between upstream connection attempts.
const MAX_BACKOFF: Duration = Duration::from_secs(5);

/// How many times the daemon connection is retried before the client is told the
/// daemon is unreachable.
const UPSTREAM_ATTEMPTS: u32 = 5;

/// The state every downstream connection shares.
#[derive(Debug)]
pub struct AppState {
    /// The daemon's event socket, opened once per downstream client.
    pub socket: PathBuf,
    /// The bearer token the read connections authenticate with.
    pub user_token: Arc<SecretString>,
    /// Whether a decision may be forwarded on the authority connection.
    pub allow_approve: bool,
    /// The publish task's request channel.
    pub uplink: mpsc::Sender<PublishRequest>,
}

/// Serves the downstream event API on `listener` until `shutdown`.
///
/// # Errors
///
/// Returns [`RunError::Serve`] if the listener fails.
pub async fn serve(
    listener: TcpListener,
    state: Arc<AppState>,
    shutdown: watch::Receiver<bool>,
) -> Result<(), RunError> {
    let app = Router::new()
        .route("/events", any(events_handler))
        .with_state(state);
    axum::serve(listener, app)
        .with_graceful_shutdown(until_shutdown(shutdown))
        .await?;
    Ok(())
}

/// Resolves when `shutdown` becomes `true` or its sender is dropped.
async fn until_shutdown(mut shutdown: watch::Receiver<bool>) {
    loop {
        if *shutdown.borrow() {
            return;
        }
        if shutdown.changed().await.is_err() {
            return;
        }
    }
}

/// Query parameters of the downstream event stream.
#[derive(Debug, Deserialize)]
struct Resume {
    /// The one-based log position to resume from, inclusive. Passed through to
    /// the daemon unchanged.
    from: Option<Seq>,
}

/// Upgrades `GET /events` connections to bidirectional event streams.
async fn events_handler(
    State(state): State<Arc<AppState>>,
    Query(resume): Query<Resume>,
    upgrade: WebSocketUpgrade,
) -> Response {
    upgrade.on_upgrade(move |socket| handle_socket(socket, state, resume.from))
}

/// Relays one client for the life of its connection.
async fn handle_socket(
    socket: WebSocket,
    state: Arc<AppState>,
    from: Option<Seq>,
) {
    let (mut sink, mut inbound) = socket.split();
    let mut upstream = match connect_upstream(&state, from).await {
        Ok(client) => client,
        Err(error) => {
            let _ = sink
                .send(notice(UPSTREAM_LOST, json!({ "error": error.to_string() })))
                .await;
            return;
        },
    };
    let (replies_tx, mut replies) = mpsc::channel::<Message>(VERDICT_CAPACITY);
    loop {
        tokio::select! {
            incoming = upstream.next() => {
                match incoming {
                    Ok(Some(envelope)) => {
                        if sink.send(wire(&envelope)).await.is_err() {
                            return;
                        }
                    },
                    Ok(None) => {
                        lost(&mut sink, "the daemon closed the connection").await;
                        return;
                    },
                    Err(error) => {
                        lost(&mut sink, &error.to_string()).await;
                        return;
                    },
                }
            },
            frame = inbound.next() => {
                match frame {
                    Some(Ok(Message::Text(text))) => {
                        if !forward(&state, &text, &replies_tx, &mut sink).await {
                            return;
                        }
                    },
                    Some(Ok(Message::Close(_))) | None => return,
                    Some(Ok(_)) => {},
                    Some(Err(error)) => {
                        tracing::debug!(%error, "a downstream frame failed");
                        return;
                    },
                }
            },
            verdict = replies.recv() => {
                let Some(verdict) = verdict else {
                    return;
                };
                if sink.send(verdict).await.is_err() {
                    return;
                }
            },
        }
    }
}

/// Routes one downstream frame. Returns `false` when the connection must close.
async fn forward(
    state: &Arc<AppState>,
    text: &str,
    replies: &mpsc::Sender<Message>,
    sink: &mut SplitSink<WebSocket, Message>,
) -> bool {
    if text.len() > MAX_FRAME_BYTES {
        return refuse(sink, None, "the frame exceeds the 64 KiB limit").await;
    }
    let event = match serde_json::from_str::<Event>(text) {
        Ok(event) => event,
        Err(error) => {
            let reason = format!("the frame is not a CloudEvent: {error}");
            return refuse(sink, None, &reason).await;
        },
    };
    let Some(route) = route(&event.r#type, state.allow_approve) else {
        let reason = if state.allow_approve {
            "the event type is reserved and is not a permission decision"
        } else {
            "the event type is reserved; run with --allow-approve to answer a \
             permission request"
        };
        return refuse(sink, Some(&event.id), reason).await;
    };
    let request = PublishRequest {
        event,
        route,
        reply: replies.clone(),
    };
    state.uplink.send(request).await.is_ok()
}

/// Answers a frame the client server will not forward.
///
/// `event_id` is `None` when the frame could not be parsed, so the client has no
/// id to correlate with.
async fn refuse(
    sink: &mut SplitSink<WebSocket, Message>,
    event_id: Option<&str>,
    error: &str,
) -> bool {
    let id = event_id.map_or(Value::Null, |id| Value::String(id.to_owned()));
    sink.send(notice(
        PUBLISH_FAILED,
        json!({ "event_id": id, "error": error }),
    ))
    .await
    .is_ok()
}

/// Tells the client the daemon connection ended, best-effort.
async fn lost(
    sink: &mut SplitSink<WebSocket, Message>,
    error: &str,
) {
    let _ = sink
        .send(notice(UPSTREAM_LOST, json!({ "error": error })))
        .await;
}

/// Opens the client's own connection to the daemon.
///
/// Retries with backoff for a bounded number of attempts, so a client that
/// arrives while the daemon is still starting connects instead of being told the
/// daemon is down.
async fn connect_upstream(
    state: &AppState,
    from: Option<Seq>,
) -> Result<WsClient, ClientError> {
    let mut backoff = INITIAL_BACKOFF;
    let mut attempt = 0_u32;
    loop {
        match WsClient::connect(&state.socket, from, state.user_token.expose_secret()).await {
            Ok(client) => return Ok(client),
            Err(error) if attempt < UPSTREAM_ATTEMPTS => {
                attempt += 1;
                tracing::debug!(%error, attempt, "retrying the daemon connection");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
            },
            Err(error) => return Err(error),
        }
    }
}
