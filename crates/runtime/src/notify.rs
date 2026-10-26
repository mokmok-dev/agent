//! Telling the supervisor that the loop is alive.
//!
//! systemd and launchd both want a heartbeat that only a working process can
//! send. The ping goes out after a loop iteration completes, so a hang stops the
//! pings and the supervisor restarts the process, which is the only failure this
//! signal can actually distinguish.

use std::ffi::OsStr;
use std::io;
use std::os::unix::net::UnixDatagram;

/// The process started and is ready to accept work.
pub const READY: &str = "READY=1";

/// The loop completed an iteration.
pub const WATCHDOG: &str = "WATCHDOG=1";

/// Sends one datagram to the socket the supervisor named.
///
/// Returns whether anything was sent, so a test can assert that a daemon run by
/// hand stays silent.
///
/// # Errors
///
/// Returns the socket error when the datagram cannot be delivered. A supervisor
/// that has gone away is not worth crashing for, so the caller decides.
pub fn notify(
    socket: Option<&OsStr>,
    message: &str,
) -> io::Result<bool> {
    let Some(socket) = socket else {
        return Ok(false);
    };
    send_to(socket, message)?;
    Ok(true)
}

fn send_to(
    socket: &OsStr,
    message: &str,
) -> io::Result<()> {
    let sender = UnixDatagram::unbound()?;
    sender.send_to_addr(message.as_bytes(), &address(socket)?)?;
    Ok(())
}

/// The socket address a supervisor named.
///
/// systemd uses the abstract namespace, written with a leading `@`, which is not
/// a file path. Everything else is a path on disk.
///
/// Linux only. The supervisor this talks to is systemd, so a build for another
/// system does not get to pretend it has one: launchd's equivalent is a later
/// change, and until it exists, code that could only ever be dead on the machine
/// the gate runs on is code the mutation gate cannot measure.
fn address(socket: &OsStr) -> io::Result<std::os::unix::net::SocketAddr> {
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::net::SocketAddr;

    socket.as_bytes().strip_prefix(b"@").map_or_else(
        || SocketAddr::from_pathname(socket),
        SocketAddr::from_abstract_name,
    )
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixDatagram;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    use super::{READY, WATCHDOG, notify};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn socket_path(name: &str) -> PathBuf {
        let unique = COUNTER.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!(
            "agent-notify-{name}-{}-{unique}",
            std::process::id()
        ))
    }

    /// A listening socket that gives up quickly, so a message that never arrives
    /// fails the test in a quarter second rather than hanging the mutation run.
    fn listener(name: &str) -> (PathBuf, UnixDatagram) {
        let path = socket_path(name);
        let _ = std::fs::remove_file(&path);
        let socket = UnixDatagram::bind(&path).expect("binds a socket");
        socket
            .set_read_timeout(Some(Duration::from_millis(250)))
            .expect("sets a read timeout");
        (path, socket)
    }

    fn receive(socket: &UnixDatagram) -> String {
        let mut buffer = [0_u8; 64];
        let read = socket.recv(&mut buffer).expect("receives the datagram");
        String::from_utf8_lossy(&buffer[..read]).to_string()
    }

    #[test]
    fn a_message_reaches_the_socket_the_supervisor_named() {
        let (path, socket) = listener("ready");
        assert!(notify(Some(path.as_os_str()), READY).expect("sends"));
        assert_eq!(receive(&socket), READY);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_watchdog_ping_travels_the_same_way() {
        let (path, socket) = listener("watchdog");
        assert!(notify(Some(path.as_os_str()), WATCHDOG).expect("sends"));
        assert_eq!(receive(&socket), WATCHDOG);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_daemon_run_by_hand_sends_nothing() {
        assert!(!notify(None, READY).expect("does nothing"));
    }

    #[test]
    fn a_socket_that_is_not_there_is_an_error_the_caller_may_ignore() {
        let path = socket_path("absent");
        let _ = std::fs::remove_file(&path);
        assert!(notify(Some(path.as_os_str()), READY).is_err());
    }

    #[test]
    fn a_second_message_is_a_second_datagram() {
        let (path, socket) = listener("twice");
        notify(Some(path.as_os_str()), READY).expect("sends");
        notify(Some(path.as_os_str()), WATCHDOG).expect("sends");
        assert_eq!(receive(&socket), READY);
        assert_eq!(receive(&socket), WATCHDOG);
        let _ = std::fs::remove_file(&path);
    }
}
