//! The child-side egress forwarder: a dumb bridge from loopback to the daemon's
//! Unix-socket proxy.
//!
//! A command inside a private network namespace has no IP route, so it cannot
//! reach a proxy on the host's loopback. But a Unix socket is a filesystem
//! object and crosses the namespace boundary. This forwarder listens on loopback
//! inside the namespace and pipes every connection to the mounted socket, so the
//! command's `HTTP_PROXY` can point at `127.0.0.1` as usual.
//!
//! It is deliberately **dumb**: it knows exactly one destination (the socket) and
//! carries no allowlist or decision. The daemon's proxy on the other end of the
//! socket is the trust boundary. It never reads the payload beyond copying bytes.

use std::io;
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::thread;

/// Runs the forwarder as a standalone bridge: listen on `127.0.0.1:<port>` and
/// forward to `socket`, until the process is killed.
///
/// # Errors
///
/// Returns an I/O error if the loopback port cannot be bound.
pub fn run(
    port: u16,
    socket: &Path,
) -> io::Result<()> {
    let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port)))?;
    accept_loop(listener, socket.to_path_buf());
    // Serve until the process is killed.
    thread::park();
    Ok(())
}

/// Runs `command` under the forwarder.
///
/// The listener is bridged in a background thread and the command runs to
/// completion, so every connection the command makes to `127.0.0.1:<port>`
/// reaches the proxy on the other end of `socket`. Returns the command's exit
/// code.
///
/// # Errors
///
/// Returns an I/O error if the port cannot be bound or the command cannot be
/// spawned.
pub fn run_command(
    port: u16,
    socket: &Path,
    command: &[String],
) -> io::Result<i32> {
    let Some((program, args)) = command.split_first() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "no command to run",
        ));
    };
    let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port)))?;
    accept_loop(listener, socket.to_path_buf());
    let status = std::process::Command::new(program).args(args).status()?;
    Ok(status.code().unwrap_or(1))
}

/// Spawns the accept thread; it forwards every loopback connection to `socket`.
fn accept_loop(
    listener: TcpListener,
    socket: PathBuf,
) {
    thread::spawn(move || {
        for client in listener.incoming() {
            let Ok(client) = client else {
                continue;
            };
            let socket = socket.clone();
            thread::spawn(move || {
                if let Err(error) = bridge(client, &socket) {
                    eprintln!("[agentd-egress-forward] {error}");
                }
            });
        }
    });
}

/// Bridges one loopback connection to the Unix socket, copying both ways.
fn bridge(
    client: TcpStream,
    socket: &Path,
) -> io::Result<()> {
    let upstream = UnixStream::connect(socket)?;
    let mut client_read = client.try_clone()?;
    let mut upstream_write = upstream.try_clone()?;
    let to_upstream = thread::spawn(move || {
        let _ = io::copy(&mut client_read, &mut upstream_write);
        let _ = upstream_write.shutdown(std::net::Shutdown::Write);
    });
    let mut upstream_read = upstream;
    let mut client_write = client;
    let _ = io::copy(&mut upstream_read, &mut client_write);
    let _ = client_write.shutdown(std::net::Shutdown::Write);
    let _ = to_upstream.join();
    Ok(())
}

/// The forwarder's command line: `--port <port> --socket <path> [-- command...]`.
#[derive(Debug, PartialEq, Eq)]
pub struct Invocation {
    /// The loopback port the forwarder listens on.
    pub port: u16,
    /// The mounted proxy socket.
    pub socket: PathBuf,
    /// A command to run under the forwarder, if any.
    pub command: Vec<String>,
}

/// Parses the forwarder's arguments.
///
/// # Errors
///
/// Returns a message when an argument is missing or malformed.
pub fn parse_args(args: &[String]) -> Result<Invocation, String> {
    let mut port = None;
    let mut socket = None;
    let mut command = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--port" => {
                let value = iter.next().ok_or("--port needs a value")?;
                port = Some(value.parse().map_err(|_| "--port needs a u16")?);
            },
            "--socket" => {
                socket = Some(PathBuf::from(iter.next().ok_or("--socket needs a value")?));
            },
            "--" => {
                command.extend(iter.by_ref().cloned());
                break;
            },
            other => return Err(format!("unknown argument {other}")),
        }
    }
    match (port, socket) {
        (Some(port), Some(socket)) => Ok(Invocation {
            port,
            socket,
            command,
        }),
        _ => Err("usage: agentd-egress-forward --port <port> --socket <path> [-- cmd...]".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::{Invocation, parse_args, run};
    use std::io::{Read as _, Write as _};
    use std::net::{Ipv4Addr, TcpStream};
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;
    use std::thread;

    #[test]
    fn bridges_loopback_to_the_socket() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("proxy.sock");
        let listener = UnixListener::bind(&socket).expect("bind socket");
        // The socket peer echoes once, standing in for the daemon proxy.
        let peer = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buffer = [0_u8; 32];
            let read = stream.read(&mut buffer).expect("read");
            stream.write_all(&buffer[..read]).expect("write");
        });

        // A fixed loopback port for the test.
        let port = 31_829;
        let server = thread::spawn(move || run(port, &socket));

        // Give the listener a moment to bind, then talk to it.
        thread::sleep(std::time::Duration::from_millis(100));
        let mut client =
            TcpStream::connect((Ipv4Addr::LOCALHOST, port)).expect("connect forwarder");
        client.write_all(b"ping").expect("write");
        let mut echo = [0_u8; 4];
        client.read_exact(&mut echo).expect("read");
        assert_eq!(&echo, b"ping");

        peer.join().expect("peer joins");
        drop(server);
    }

    #[test]
    fn parses_the_port_socket_and_command() {
        let args = vec![
            String::from("--port"),
            String::from("3128"),
            String::from("--socket"),
            String::from("/run/proxy.sock"),
            String::from("--"),
            String::from("bash"),
            String::from("-c"),
            String::from("echo hi"),
        ];
        assert_eq!(
            parse_args(&args),
            Ok(Invocation {
                port: 3128,
                socket: PathBuf::from("/run/proxy.sock"),
                command: vec![
                    String::from("bash"),
                    String::from("-c"),
                    String::from("echo hi")
                ],
            })
        );
    }

    #[test]
    fn rejects_a_missing_or_bad_argument() {
        assert!(parse_args(&[]).is_err());
        assert!(parse_args(&[String::from("--port"), String::from("x")]).is_err());
        assert!(parse_args(&[String::from("--socket")]).is_err());
    }
}
