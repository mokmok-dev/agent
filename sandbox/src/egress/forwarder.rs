//! The child-side forwarder: a dumb bridge from loopback TCP to the proxy socket.
//!
//! A command inside a private network namespace has no IP route, so it cannot
//! reach the daemon's proxy socket by address. The socket is a filesystem object
//! that crosses the namespace, and the command's own loopback is the one address
//! it has. The forwarder listens on that loopback and copies every connection to
//! the mounted socket, so the command's `HTTP_PROXY` can name `127.0.0.1` as it
//! would outside a sandbox.
//!
//! It is deliberately **dumb**: it knows exactly one destination — the socket —
//! and carries no allowlist or decision. The proxy on the other end of the socket
//! is the trust boundary, and the forwarder never parses the payload beyond
//! copying bytes, so the decision cannot be bypassed by confusing it. See
//! `docs/sandbox/network.md`.
//!
//! Starting the forwarder alongside the command inside the namespace is the
//! supervisor's job; this module only binds, bridges, and parses its arguments.

use std::ffi::OsString;
use std::io;
use std::net::{Ipv4Addr, TcpListener};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use super::tunnel;

/// Where the forwarder listens and where it forwards.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForwardConfig {
    /// The loopback port the command's `HTTP_PROXY` names.
    pub port: u16,
    /// The daemon's proxy socket, mounted into the command's namespace.
    pub socket: PathBuf,
}

/// Why a forwarder configuration could not be built.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ForwardError {
    /// The `--socket` argument was not given.
    #[error("the forwarder needs --socket <path>")]
    MissingSocket,
    /// The `--port` argument was not given.
    #[error("the forwarder needs --port <port>")]
    MissingPort,
    /// The `--port` argument was not a number in `0..=65535`.
    #[error("`{value}` is not a valid port")]
    BadPort {
        /// The rejected value.
        value: String,
    },
    /// An argument was neither `--socket` nor `--port`.
    #[error("unknown argument `{argument}`")]
    UnknownArgument {
        /// The rejected argument.
        argument: String,
    },
}

impl ForwardConfig {
    /// Parse `--socket <path>` and `--port <port>`.
    ///
    /// The parser is deliberately tiny and dependency-free: the forwarder is
    /// std-only so it stays cheap to build and audit, and two flags do not
    /// justify a parser crate. A socket path is taken as an `OsString`, so a
    /// non-UTF-8 path survives.
    ///
    /// Port `0` is accepted and means "let the OS choose": the forwarder reports
    /// the bound port on stderr, so a supervisor can read it and put it in the
    /// command's `HTTP_PROXY`. A concrete port is the usual case, because the
    /// policy's `ProxyGrant.port` already names one.
    ///
    /// # Errors
    ///
    /// Returns a [`ForwardError`] for a missing, unknown, or malformed argument.
    pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Self, ForwardError> {
        let mut socket = None;
        let mut port = None;
        let mut args = args.into_iter();
        while let Some(argument) = args.next() {
            match argument.to_str() {
                Some("--socket") => {
                    socket = Some(PathBuf::from(
                        args.next().ok_or(ForwardError::MissingSocket)?,
                    ));
                },
                Some("--port") => {
                    let value = args.next().ok_or(ForwardError::MissingPort)?;
                    let text = value.to_str().ok_or_else(|| ForwardError::BadPort {
                        value: value.to_string_lossy().into_owned(),
                    })?;
                    port = Some(text.parse().map_err(|_| ForwardError::BadPort {
                        value: text.to_owned(),
                    })?);
                },
                _ => {
                    return Err(ForwardError::UnknownArgument {
                        argument: argument.to_string_lossy().into_owned(),
                    });
                },
            }
        }
        let socket = socket.ok_or(ForwardError::MissingSocket)?;
        let port = port.ok_or(ForwardError::MissingPort)?;
        Ok(Self { port, socket })
    }
}

/// A running forwarder: a loopback listener bridged to the proxy socket.
#[derive(Debug)]
pub struct Forwarder {
    listener: TcpListener,
    socket: PathBuf,
    port: u16,
}

impl Forwarder {
    /// Bind the loopback listener for `config`.
    ///
    /// Port `0` binds an ephemeral port, which [`Forwarder::port`] then reports.
    /// A daemon passes a concrete port so the command's `HTTP_PROXY` can name it.
    ///
    /// # Errors
    ///
    /// Returns the underlying [`io::Error`] if the loopback port cannot be bound
    /// or its address cannot be read.
    pub fn bind(config: &ForwardConfig) -> io::Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, config.port))?;
        // The bound port is read once here, so `port` cannot fail later and a
        // caller has no error branch to handle for a socket it just bound.
        let port = listener.local_addr()?.port();
        Ok(Self {
            listener,
            socket: config.socket.clone(),
            port,
        })
    }

    /// The bound loopback port.
    #[must_use]
    pub const fn port(&self) -> u16 {
        self.port
    }

    /// Accept one connection and bridge it to the proxy socket.
    ///
    /// # Errors
    ///
    /// Returns the underlying [`io::Error`] if accepting, connecting the socket,
    /// or the bridge fails.
    pub fn serve_one(&self) -> io::Result<()> {
        let (client, _) = self.listener.accept()?;
        let upstream = UnixStream::connect(&self.socket)?;
        tunnel(&client, &upstream)
    }

    /// Serve connections until the process ends.
    ///
    /// Each connection is bridged on its own thread, so one long-lived tunnel
    /// does not stop the next connection being served. A connection that cannot
    /// be bridged is dropped, never treated as a reason to stop: the forwarder
    /// holds no decision, so it has nothing to decide to stop for.
    pub fn run(&self) {
        loop {
            match self.listener.accept() {
                Ok((client, _)) => {
                    let socket = self.socket.clone();
                    std::thread::spawn(move || {
                        if let Ok(upstream) = UnixStream::connect(&socket) {
                            let _ = tunnel(&client, &upstream);
                        }
                    });
                },
                Err(error) => eprintln!("egress-forward: accept failed: {error}"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    // Tests for the forwarder's argument parser, its loopback binding, and one
    // served connection. The full byte bridge is covered by the integration test.

    use std::io::{Read as _, Write as _};
    use std::net::TcpStream;
    use std::os::unix::net::UnixListener;
    use std::time::Duration;

    use super::*;

    /// `parts` as the `OsString` arguments a process would receive.
    fn args(parts: &[&str]) -> Vec<OsString> {
        parts.iter().map(OsString::from).collect()
    }

    #[test]
    fn a_socket_and_port_parse() {
        let config =
            ForwardConfig::parse(args(&["--socket", "/run/egress.sock", "--port", "8080"]))
                .expect("parses");
        assert_eq!(config.port, 8080);
        assert_eq!(config.socket, PathBuf::from("/run/egress.sock"));
    }

    #[test]
    fn the_order_of_arguments_does_not_matter() {
        let config =
            ForwardConfig::parse(args(&["--port", "1", "--socket", "/s"])).expect("parses");
        assert_eq!(config.port, 1);
        assert_eq!(config.socket, PathBuf::from("/s"));
    }

    #[test]
    fn a_missing_socket_is_rejected() {
        assert_eq!(
            ForwardConfig::parse(args(&["--port", "8080"])),
            Err(ForwardError::MissingSocket)
        );
    }

    #[test]
    fn a_missing_port_is_rejected() {
        assert_eq!(
            ForwardConfig::parse(args(&["--socket", "/s"])),
            Err(ForwardError::MissingPort)
        );
    }

    #[test]
    fn a_non_numeric_port_is_rejected() {
        assert!(matches!(
            ForwardConfig::parse(args(&["--socket", "/s", "--port", "x"])),
            Err(ForwardError::BadPort { .. })
        ));
    }

    #[test]
    fn a_port_above_the_maximum_is_rejected() {
        assert!(matches!(
            ForwardConfig::parse(args(&["--socket", "/s", "--port", "65536"])),
            Err(ForwardError::BadPort { .. })
        ));
    }

    #[test]
    fn port_zero_means_let_the_os_choose() {
        // A supervisor may pass 0 and read the bound port back from the binary,
        // so this must parse rather than be rejected.
        let config = ForwardConfig::parse(args(&["--socket", "/s", "--port", "0"]))
            .expect("port zero parses");
        assert_eq!(config.port, 0);
    }

    #[test]
    fn an_unknown_argument_is_rejected() {
        assert!(matches!(
            ForwardConfig::parse(args(&["--socket", "/s", "--port", "1", "--oops"])),
            Err(ForwardError::UnknownArgument { .. })
        ));
    }

    #[test]
    fn binding_port_zero_reports_a_real_ephemeral_port() {
        let config = ForwardConfig {
            port: 0,
            socket: PathBuf::from("/nonexistent.sock"),
        };
        let forwarder = Forwarder::bind(&config).expect("binds an ephemeral port");
        assert_ne!(forwarder.port(), 0);
    }

    #[test]
    fn serve_one_bridges_one_connection_to_the_socket() {
        // Covers `serve_one` directly. The socket replies a fixed banner and then
        // drops its end; the client half-closes after reading it, so both copy
        // directions reach EOF and the bridge returns rather than waiting.
        let root = std::env::temp_dir().join(format!("sandbox-forward-one-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("creates the root");
        let socket = root.join("e.sock");
        let listener = UnixListener::bind(&socket).expect("binds the socket");
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let _ = stream.write_all(b"BANNER\n");
                // Dropping `stream` closes it, so the bridge sees EOF upstream.
            }
        });

        let config = ForwardConfig { port: 0, socket };
        let forwarder = Forwarder::bind(&config).expect("binds the forwarder");
        let port = forwarder.port();
        let serving = std::thread::spawn(move || forwarder.serve_one());

        let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).expect("connects");
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("sets a read deadline");
        let mut banner = [0_u8; 32];
        let read = client.read(&mut banner).expect("reads the banner");
        assert_eq!(&banner[..read], b"BANNER\n");
        // Half-close so the client->socket direction ends and the bridge returns.
        client
            .shutdown(std::net::Shutdown::Write)
            .expect("half-closes");

        serving.join().expect("joins").expect("serves one");
        let _ = std::fs::remove_dir_all(&root);
    }
}
