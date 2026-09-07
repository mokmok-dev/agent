use thiserror::Error;
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

/// Serves an HTTP server over the Unix domain socket at `socket` until a
/// SIGINT or SIGTERM is received.
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
pub async fn run(socket: std::path::PathBuf) -> Result<(), ServerError> {
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

    let router = axum::Router::new().layer(TraceLayer::new_for_http());

    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;

    let () = axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            tokio::select! {
                _ = interrupt.recv() => {},
                _ = terminate.recv() => {},
            }
        })
        .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{ServerError, run};

    #[tokio::test]
    async fn run_reports_already_running_when_socket_is_live() -> Result<(), ServerError> {
        let dir = tempfile::tempdir()?;
        let socket = dir.path().join("test.sock");
        let _listener = tokio::net::UnixListener::bind(&socket)?;

        assert!(matches!(
            run(socket).await,
            Err(ServerError::AlreadyRunning(_))
        ));

        Ok(())
    }
}
