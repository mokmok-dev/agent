//! A WebSocket client for the daemon's event API over a Unix domain socket.
//!
//! The client is bidirectional: it receives [`WireMessage`]s (an [`Event`]
//! paired with its log position) and can publish [`Event`]s. Connecting with a
//! resume position makes the daemon replay history from that position before
//! the live stream continues, so a node can rejoin without gaps or duplicates.

use agentd_events::{Event, Seq, WireMessage};
use futures_util::{SinkExt, StreamExt};
use secrecy::zeroize::Zeroizing;
use std::path::Path;
use thiserror::Error;
use tokio::net::UnixStream;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

/// Errors returned by [`WsClient`].
#[derive(Debug, Error)]
pub enum ClientError {
    /// Connecting to the socket failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// The WebSocket handshake or a frame failed.
    #[error(transparent)]
    WebSocket(#[from] tokio_tungstenite::tungstenite::Error),
    /// The bearer token could not be encoded as a header value.
    #[error("the bearer token is not a valid header value")]
    Token,
    /// A received frame was not a valid [`WireMessage`].
    #[error("the daemon sent an invalid wire message: {0}")]
    Decode(#[source] serde_json::Error),
    /// An [`Event`] to publish could not be serialized.
    #[error("the event could not be serialized: {0}")]
    Encode(#[source] serde_json::Error),
}

/// A bidirectional WebSocket connection to the daemon.
#[derive(Debug)]
pub struct WsClient {
    stream: WebSocketStream<UnixStream>,
}

impl WsClient {
    /// Connects to the daemon's `/events` endpoint over the Unix domain socket
    /// at `socket`, authenticating with `token`.
    ///
    /// When `from` is `Some(position)`, the daemon replays history from that
    /// position (inclusive) before continuing live.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError::Io`] if the socket cannot be reached,
    /// [`ClientError::Token`] if the token is not a valid header value, and
    /// [`ClientError::WebSocket`] if the handshake fails (including a rejected
    /// token).
    pub async fn connect(
        socket: &Path,
        from: Option<Seq>,
        token: &str,
    ) -> Result<Self, ClientError> {
        let stream = UnixStream::connect(socket).await?;
        let url = from.map_or_else(
            || String::from("ws://localhost/events"),
            |position| format!("ws://localhost/events?from={position}"),
        );
        let mut request = url.as_str().into_client_request()?;
        request.headers_mut().insert(
            "authorization",
            Zeroizing::new(format!("Bearer {token}"))
                .parse()
                .map_err(|_| ClientError::Token)?,
        );
        let (stream, _response) = tokio_tungstenite::client_async(request, stream).await?;
        Ok(Self { stream })
    }

    /// Waits for the next wire message.
    ///
    /// Returns `Ok(None)` when the daemon closes the connection; control and
    /// binary frames are ignored.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError::Decode`] if a text frame is not a valid
    /// [`WireMessage`] and [`ClientError::WebSocket`] on a transport failure.
    pub async fn next(&mut self) -> Result<Option<WireMessage>, ClientError> {
        loop {
            match self.stream.next().await {
                None | Some(Ok(Message::Close(_))) => return Ok(None),
                Some(Ok(Message::Text(text))) => {
                    let wire = serde_json::from_str(text.as_str()).map_err(ClientError::Decode)?;
                    return Ok(Some(wire));
                },
                Some(Ok(_)) => {},
                Some(Err(error)) => return Err(error.into()),
            }
        }
    }

    /// Sends `event` to the daemon.
    ///
    /// This returns once the frame has been written to the socket. The daemon's
    /// durable-append result is not acknowledged on this path: a rejected event
    /// is reported as an `error.invalid_event` or `error.publish_failed` notice
    /// on the receive side, and a committed event comes back on the live stream.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError::Encode`] if the event cannot be serialized and
    /// [`ClientError::WebSocket`] if the frame cannot be sent.
    pub async fn send(
        &mut self,
        event: &Event,
    ) -> Result<(), ClientError> {
        let text = serde_json::to_string(event).map_err(ClientError::Encode)?;
        self.stream.send(Message::text(text)).await?;
        Ok(())
    }
}
