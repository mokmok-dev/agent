//! The daemon's connection to the event bus.
//!
//! The daemon is a bus peer: it holds one connection over the bus's Unix domain
//! socket and uses it to subscribe to the events it must act on, to publish its
//! own lifecycle events, and to answer an egress approval request. See
//! `docs/session/daemon.md`.
//!
//! The transport is a WebSocket, and one connection must both read (a long-lived
//! subscription) and write (a publish, an ack), so the connection runs on its own
//! thread with a current-thread runtime. The public methods are **blocking**,
//! because the daemon and the sandbox are blocking: the session core is
//! synchronous and the egress `Approver::ask` runs on the proxy's thread. tokio
//! is this module's private implementation detail.
//!
//! The [`Publisher`] bridge is how the sandbox reaches the bus: the sandbox
//! authors an event, the bridge maps it to the producer half of a `CloudEvents`
//! envelope, and the client publishes it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::mpsc::SyncSender;

use agent::cloudevent::{Incoming, SpecVersion};
use agent::protocol::{ClientMessage, ErrorCode, ServerMessage, parse, to_text};
use agent::transport::SUBPROTOCOL;
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderValue, header};
use ulid::Ulid;

/// What a bus connection needs to open.
#[derive(Debug, Clone)]
pub struct Config {
    /// The bus's Unix domain socket path.
    pub socket: PathBuf,
    /// The stable subscriber identity the durable cursor is keyed on. Fixed for
    /// the daemon.
    pub subscriber_id: String,
}

/// A receipt for a committed event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Receipt {
    /// The sequence number the bus assigned.
    pub seq: u64,
}

/// A closure the connection pumps committed events to.
///
/// It runs on the connection's thread, so it must not block. A handler that does
/// real work stalls the read loop, the bus's socket write blocks, and the bus
/// eventually evicts this subscriber as slow. Forward to a queue the daemon's own
/// loop drains, rather than doing the work here.
pub type Handler = Box<dyn Fn(&agent::cloudevent::Event) + Send + 'static>;

/// The daemon's single connection to the bus.
pub struct BusClient {
    commands: mpsc::UnboundedSender<Command>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl std::fmt::Debug for BusClient {
    fn fmt(
        &self,
        f: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        f.debug_struct("BusClient").finish_non_exhaustive()
    }
}

/// A request sent to the connection thread, carrying its own reply channel.
enum Command {
    Publish {
        event: Box<Incoming>,
        reply: SyncSender<Result<Receipt, Error>>,
    },
    Subscribe {
        from_seq: u64,
        handler: Handler,
        reply: SyncSender<Result<u64, Error>>,
    },
    Ack {
        cursor: u64,
        reply: SyncSender<Result<(), Error>>,
    },
    Stop,
}

impl BusClient {
    /// Open the socket, complete the handshake, and start the connection thread.
    ///
    /// Blocks until the handshake succeeds or fails, so a caller learns
    /// immediately whether the bus is reachable.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] or [`Error::WebSocket`] if the socket cannot be
    /// opened or the handshake fails, and [`Error::Handshake`] if the bus does
    /// not accept the subprotocol.
    pub fn connect(config: &Config) -> Result<Self, Error> {
        let (commands, receiver) = mpsc::unbounded_channel();
        let (ready_send, ready_recv) = std::sync::mpsc::channel();
        let socket = config.socket.clone();
        let subscriber_id = config.subscriber_id.clone();
        let thread = std::thread::Builder::new()
            .name("daemon-bus".to_owned())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = ready_send.send(Err(Error::Io(error)));
                        return;
                    },
                };
                runtime.block_on(async move {
                    match open_socket(&socket).await {
                        Ok(socket) => {
                            let _ = ready_send.send(Ok(()));
                            run(socket, receiver, subscriber_id).await;
                        },
                        Err(error) => {
                            let _ = ready_send.send(Err(error));
                        },
                    }
                });
            })
            .map_err(Error::Io)?;

        match ready_recv.recv() {
            Ok(Ok(())) => Ok(Self {
                commands,
                thread: Mutex::new(Some(thread)),
            }),
            Ok(Err(error)) => {
                let _ = thread.join();
                Err(error)
            },
            Err(_) => {
                let _ = thread.join();
                Err(Error::Closed)
            },
        }
    }

    /// Publish an authored event, blocking until the bus has committed it.
    ///
    /// The event is durable when this returns: the bus only replies
    /// [`ServerMessage::Published`] after its write-ahead log's `fsync`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Refused`] when the bus rejects the request (for example a
    /// producer-set bus attribute, or a type the connection may not publish),
    /// and [`Error::Closed`] if the connection is gone.
    pub fn publish(
        &self,
        event: Incoming,
    ) -> Result<Receipt, Error> {
        let (reply, receiver) = std::sync::mpsc::sync_channel(1);
        self.commands
            .send(Command::Publish {
                event: Box::new(event),
                reply,
            })
            .map_err(|_| Error::Closed)?;
        receiver.recv().unwrap_or(Err(Error::Closed))
    }

    /// Subscribe as the configured subscriber, replaying from `from_seq`.
    ///
    /// The bus resolves the start to `max(from_seq, stored cursor)` and returns
    /// it. The handler then receives replayed history followed by live events.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Refused`] when the bus rejects the request (for example
    /// an invalid subscriber id), and [`Error::Closed`] if the connection is
    /// gone.
    pub fn subscribe(
        &self,
        from_seq: u64,
        handler: Handler,
    ) -> Result<u64, Error> {
        let (reply, receiver) = std::sync::mpsc::sync_channel(1);
        self.commands
            .send(Command::Subscribe {
                from_seq,
                handler,
                reply,
            })
            .map_err(|_| Error::Closed)?;
        receiver.recv().unwrap_or(Err(Error::Closed))
    }

    /// Durably record the last processed sequence.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Refused`] or [`Error::Closed`].
    pub fn ack(
        &self,
        cursor: u64,
    ) -> Result<(), Error> {
        let (reply, receiver) = std::sync::mpsc::sync_channel(1);
        self.commands
            .send(Command::Ack { cursor, reply })
            .map_err(|_| Error::Closed)?;
        receiver.recv().unwrap_or(Err(Error::Closed))
    }

    /// Ask the connection thread to stop. Idempotent.
    pub fn stop(&self) {
        let _ = self.commands.send(Command::Stop);
    }
}

impl Drop for BusClient {
    fn drop(&mut self) {
        self.stop();
        if let Ok(mut guard) = self.thread.lock()
            && let Some(thread) = guard.take()
        {
            let _ = thread.join();
        }
    }
}

/// Open the WebSocket over the Unix socket, offering the bus subprotocol.
async fn open_socket(path: &Path) -> Result<WebSocket, Error> {
    let stream = UnixStream::connect(path).await?;
    let mut request = "ws://localhost/".into_client_request()?;
    request.headers_mut().insert(
        header::SEC_WEBSOCKET_PROTOCOL,
        HeaderValue::from_static(SUBPROTOCOL),
    );
    let (socket, response) = tokio_tungstenite::client_async(request, stream).await?;

    // The bus answers with the subprotocol it selected. A response that names a
    // different one means the client and server disagree on the wire format, so
    // refuse rather than speak an unknown dialect.
    let selected = response
        .headers()
        .get(header::SEC_WEBSOCKET_PROTOCOL)
        .and_then(|value| value.to_str().ok());
    if selected != Some(SUBPROTOCOL) {
        return Err(Error::Handshake {
            selected: selected.map(str::to_owned),
        });
    }
    Ok(socket)
}

/// A connected bus WebSocket.
type WebSocket = WebSocketStream<UnixStream>;

/// The state the connection thread keeps between frames.
#[derive(Default)]
struct Pending {
    /// Publishes awaiting their `published` reply, keyed by the request id.
    publishes: HashMap<String, SyncSender<Result<Receipt, Error>>>,
    /// The subscribe request awaiting `subscribed`.
    subscribe: Option<SyncSender<Result<u64, Error>>>,
    /// The ack awaiting `cursor_ack`.
    ack: Option<SyncSender<Result<(), Error>>>,
}

/// The connection loop: serve commands and route incoming frames.
///
/// The sink is written only from the command branch and the stream read only
/// from the frame branch, so one `select!` owns both halves without a lock.
async fn run(
    socket: WebSocket,
    mut commands: mpsc::UnboundedReceiver<Command>,
    subscriber_id: String,
) {
    let (mut sink, mut stream) = socket.split();
    let mut pending = Pending::default();
    let mut handler: Option<Handler> = None;

    loop {
        tokio::select! {
            command = commands.recv() => {
                let Some(command) = command else { break };
                match command {
                    Command::Publish { event, reply } => {
                        let id = Ulid::generate().to_string();
                        let message = ClientMessage::Publish {
                            id: id.clone(),
                            event,
                            idempotency_key: None,
                        };
                        match build(&message) {
                            Ok(text) => match sink.send(Message::text(text)).await {
                                Ok(()) => { pending.publishes.insert(id, reply); }
                                Err(error) => { let _ = reply.send(Err(error.into())); }
                            },
                            Err(error) => { let _ = reply.send(Err(error)); }
                        }
                    }
                    Command::Subscribe { from_seq, handler: new, reply } => {
                        let message = ClientMessage::Subscribe {
                            subscriber_id: subscriber_id.clone(),
                            from_seq,
                            filter: None,
                        };
                        match build(&message) {
                            Ok(text) => match sink.send(Message::text(text)).await {
                                Ok(()) => {
                                    handler = Some(new);
                                    pending.subscribe = Some(reply);
                                }
                                Err(error) => { let _ = reply.send(Err(error.into())); }
                            },
                            Err(error) => { let _ = reply.send(Err(error)); }
                        }
                    }
                    Command::Ack { cursor, reply } => {
                        let message = ClientMessage::Ack { cursor };
                        match build(&message) {
                            Ok(text) => match sink.send(Message::text(text)).await {
                                Ok(()) => { pending.ack = Some(reply); }
                                Err(error) => { let _ = reply.send(Err(error.into())); }
                            },
                            Err(error) => { let _ = reply.send(Err(error)); }
                        }
                    }
                    Command::Stop => break,
                }
            }
            frame = stream.next() => {
                let Some(frame) = frame else { break };
                let Ok(frame) = frame else { break };
                if !frame.is_text() {
                    continue;
                }
                let Ok(text) = frame.into_text() else { continue };
                route(&text, &mut pending, &mut handler);
            }
        }
    }

    // Fail every waiter still outstanding, so no caller blocks on a closed
    // connection.
    for (_, reply) in pending.publishes.drain() {
        let _ = reply.send(Err(Error::Closed));
    }
    if let Some(reply) = pending.subscribe.take() {
        let _ = reply.send(Err(Error::Closed));
    }
    if let Some(reply) = pending.ack.take() {
        let _ = reply.send(Err(Error::Closed));
    }
}

/// Serialize a client message to its wire text.
fn build(message: &ClientMessage) -> Result<String, Error> {
    Ok(to_text(message)?)
}

/// Route one incoming text frame to the waiter it answers.
fn route(
    text: &str,
    pending: &mut Pending,
    handler: &mut Option<Handler>,
) {
    match parse(text) {
        Ok(agent::protocol::Message::Event(event)) => {
            if let Some(handler) = handler {
                handler(&event);
            }
        },
        Ok(agent::protocol::Message::Server(message)) => route_server(message, pending),
        // A client message or an unparseable frame is not something the server
        // sends; ignore it rather than tearing down the connection.
        Ok(agent::protocol::Message::Client(_)) | Err(_) => {},
    }
}

/// Resolve a server control message against the waiting callers.
fn route_server(
    message: ServerMessage,
    pending: &mut Pending,
) {
    match message {
        ServerMessage::Published { id, seq } => {
            if let Some(reply) = pending.publishes.remove(&id) {
                let _ = reply.send(Ok(Receipt { seq }));
            }
        },
        ServerMessage::Subscribed { from_seq } => {
            if let Some(reply) = pending.subscribe.take() {
                let _ = reply.send(Ok(from_seq));
            }
        },
        ServerMessage::CursorAck { .. } => {
            if let Some(reply) = pending.ack.take() {
                let _ = reply.send(Ok(()));
            }
        },
        ServerMessage::Error { code, message, id } => {
            let error = Error::Refused { code, message };
            if let Some(id) = id
                && let Some(reply) = pending.publishes.remove(&id)
            {
                let _ = reply.send(Err(error));
                return;
            }
            // An id-less error belongs to one of the requests that carries no id
            // of its own on the wire: the ack, or the subscribe.
            if let Some(reply) = pending.ack.take() {
                let _ = reply.send(Err(error));
                return;
            }
            if let Some(reply) = pending.subscribe.take() {
                let _ = reply.send(Err(error));
            }
        },
        // The bus reserves `gap` for range unavailability, which this client
        // does not request; ignore it.
        ServerMessage::Gap { .. } => {},
    }
}

/// The daemon's implementation of the sandbox's [`Publisher`] seam.
///
/// The sandbox authors an event and the bridge publishes it; the sandbox never
/// holds a bus handle. Ids are ULIDs, matching the ids the bus assigns.
#[derive(Debug)]
pub struct BusPublisher {
    client: std::sync::Arc<BusClient>,
}

impl BusPublisher {
    /// A publisher over `client`.
    #[must_use]
    pub const fn new(client: std::sync::Arc<BusClient>) -> Self {
        Self { client }
    }
}

impl sandbox::egress::Publisher for BusPublisher {
    fn publish(
        &self,
        event: sandbox::events::Event,
    ) {
        // The seam returns nothing, so a failure cannot propagate. Log it: a
        // refused publish means the proxy asked a question the log will not
        // show, which an operator must see.
        if let Err(error) = self.client.publish(to_incoming(event)) {
            tracing::error!(%error, "could not publish a sandbox event");
        }
    }

    fn next_request_id(&self) -> sandbox::egress::RequestId {
        sandbox::egress::RequestId::new(Ulid::generate().to_string())
    }
}

/// Map a sandbox-authored event to the producer half of an envelope.
///
/// Built as a struct literal, not by parsing, so `source` and `sequence` are
/// provably `None`: the bus owns them and rejects a producer that sets them.
#[must_use]
pub fn to_incoming(event: sandbox::events::Event) -> Incoming {
    authored(event.ty, event.subject, event.data, event.traceparent)
}

/// Build the producer half of an envelope from the daemon's own fields.
///
/// `source` and `sequence` stay `None`: the bus owns them and rejects a producer
/// that sets them. `traceparent`, when present, is carried as an extension.
#[must_use]
pub fn authored(
    ty: &str,
    subject: impl Into<String>,
    data: Value,
    traceparent: Option<String>,
) -> Incoming {
    let mut extensions = std::collections::BTreeMap::new();
    if let Some(traceparent) = traceparent {
        extensions.insert("traceparent".to_owned(), Value::String(traceparent));
    }
    Incoming {
        specversion: SpecVersion::V1_0,
        ty: ty.to_owned(),
        source: None,
        id: None,
        time: None,
        subject: Some(subject.into()),
        datacontenttype: None,
        sequence: None,
        data: Some(data),
        extensions,
    }
}

/// Failures of the bus connection.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A socket operation failed.
    #[error("bus I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// The WebSocket layer failed.
    #[error("bus websocket error: {0}")]
    WebSocket(#[from] tokio_tungstenite::tungstenite::Error),
    /// A protocol message could not be encoded or decoded.
    #[error("bus protocol error: {0}")]
    Protocol(#[from] agent::protocol::Error),
    /// The bus selected a subprotocol this client does not speak.
    #[error("the bus selected an unsupported subprotocol: {selected:?}")]
    Handshake {
        /// The subprotocol the bus selected, if any.
        selected: Option<String>,
    },
    /// The bus refused the request.
    #[error("the bus refused the request: {code:?}: {message}")]
    Refused {
        /// The machine-readable code.
        code: ErrorCode,
        /// The bus's explanation.
        message: String,
    },
    /// The connection is gone.
    #[error("the bus connection is closed")]
    Closed,
}
