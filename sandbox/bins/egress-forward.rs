//! `egress-forward`: bridge a confined command's loopback to the proxy socket.
//!
//! A command runs in a private network namespace with no IP route, so it cannot
//! reach the daemon's Unix-socket proxy by address. This process listens on
//! loopback inside that namespace and copies every connection to the mounted
//! socket, so the command's `HTTP_PROXY` can name `127.0.0.1` as usual.
//!
//! It is deliberately dumb: it knows one destination and carries no allowlist or
//! decision. The proxy at the far end of the socket is the trust boundary. See
//! `docs/sandbox/network.md`.
//!
//! Usage: `egress-forward --socket <path> --port <port>`

use std::process::ExitCode;

use sandbox::egress::{ForwardConfig, Forwarder};

fn main() -> ExitCode {
    let config = match ForwardConfig::parse(std::env::args_os().skip(1)) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("egress-forward: {error}");
            eprintln!("usage: egress-forward --socket <path> --port <port>");
            return ExitCode::FAILURE;
        },
    };

    let forwarder = match Forwarder::bind(&config) {
        Ok(forwarder) => forwarder,
        Err(error) => {
            eprintln!(
                "egress-forward: cannot bind 127.0.0.1:{}: {error}",
                config.port
            );
            return ExitCode::FAILURE;
        },
    };

    // Report the bound port so a supervisor that asked for port 0 can read it.
    eprintln!(
        "egress-forward: listening on 127.0.0.1:{}",
        forwarder.port()
    );

    forwarder.run();
    ExitCode::SUCCESS
}
