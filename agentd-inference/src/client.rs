//! The node-side client for the daemon's `/inference` WebSocket.
//!
//! The daemon serves inference on the same Unix domain socket as the event API.
//! A client opens `/inference`, sends one [`InferenceRequest`] as a text frame,
//! and reads [`Delta`] text frames until a terminal delta. The stream is
//! transient: it is never written to the durable event log.

use std::path::Path;

use futures_util::{SinkExt, StreamExt};
use thiserror::Error;
use tokio::net::UnixStream;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use crate::wire::{Delta, InferenceRequest};

/// Errors returned by [`InferenceClient`].
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
    /// A sent request could not be serialized.
    #[error("the inference request could not be serialized: {0}")]
    Encode(#[source] serde_json::Error),
    /// A received frame was not a valid [`Delta`].
    #[error("the daemon sent an invalid delta: {0}")]
    Decode(#[source] serde_json::Error),
    /// The daemon closed the stream before a terminal delta.
    #[error("the daemon closed the inference stream before it finished")]
    Truncated,
}

/// A WebSocket client for the daemon's `/inference` endpoint.
#[derive(Debug)]
pub struct InferenceClient {
    stream: WebSocketStream<UnixStream>,
}

impl InferenceClient {
    /// Connects to `/inference` over the Unix domain socket at `socket`,
    /// authenticating with `token`.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError::Io`] if the socket cannot be reached,
    /// [`ClientError::Token`] if the token is not a valid header value, and
    /// [`ClientError::WebSocket`] if the handshake fails (including a rejected
    /// token).
    pub async fn connect(
        socket: &Path,
        token: &str,
    ) -> Result<Self, ClientError> {
        let stream = UnixStream::connect(socket).await?;
        let mut request = "ws://localhost/inference".into_client_request()?;
        request.headers_mut().insert(
            "authorization",
            format!("Bearer {token}")
                .parse()
                .map_err(|_| ClientError::Token)?,
        );
        let (stream, _response) = tokio_tungstenite::client_async(request, stream).await?;
        Ok(Self { stream })
    }

    /// Sends `request` and collects the response deltas up to and including the
    /// terminal one.
    ///
    /// A [`Delta::Error`] is returned inside the vector rather than as a
    /// [`ClientError`]: it is a model-side failure the caller decides how to
    /// handle, not a transport failure.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError::Encode`] if the request cannot be serialized,
    /// [`ClientError::Decode`] if a frame is not a valid delta, and
    /// [`ClientError::Truncated`] if the stream closes early.
    pub async fn complete(
        &mut self,
        request: &InferenceRequest,
    ) -> Result<Vec<Delta>, ClientError> {
        let text = serde_json::to_string(request).map_err(ClientError::Encode)?;
        self.stream.send(Message::text(text)).await?;

        let mut deltas = Vec::new();
        while let Some(message) = self.stream.next().await {
            match message? {
                Message::Text(text) => {
                    let delta: Delta =
                        serde_json::from_str(text.as_str()).map_err(ClientError::Decode)?;
                    let terminal = delta.is_terminal();
                    deltas.push(delta);
                    if terminal {
                        return Ok(deltas);
                    }
                },
                Message::Close(_) => break,
                _ => {},
            }
        }
        Err(ClientError::Truncated)
    }

    /// Closes the connection.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError::WebSocket`] if the close frame cannot be sent.
    pub async fn close(&mut self) -> Result<(), ClientError> {
        self.stream.close(None).await?;
        Ok(())
    }
}
