//! The approver client: it prints the requests awaiting a decision and answers
//! one of them.
//!
//! It exists because the daemon records requests, not answers: a bridged agent's
//! `session.permission.requested` and the CONNECT proxy's
//! `session.egress.requested` wait for an authority client, and the sandbox's
//! `sandbox.permission.requested` for a human. Reading uses a read token and
//! republishing the decision uses the authority token, because every request and
//! decision type is reserved to daemon-authority publishers.

use agentd_node::approval::{Decision, Pending, pending};
use agentd_node::{Event, PublishError, WsClient};
use clap::{Parser, Subcommand};
use secrecy::zeroize::Zeroizing;
use std::path::{Path, PathBuf};
use std::time::Duration;
use thiserror::Error;

/// How long to wait for the daemon to replay its log or commit a decision.
const TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Parser)]
#[command(name = "agentd-approve", version = env!("CARGO_PKG_VERSION"))]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Print the requests no approver has answered yet, then exit.
    Pending(Connection),
    /// Answer one request.
    Decide(Decide),
}

#[derive(Debug, clap::Args)]
struct Connection {
    /// The daemon's event WebSocket Unix socket.
    #[arg(long, default_value_os_t = agentd_events::paths::default_socket())]
    socket: PathBuf,
    /// A file whose entire contents is the bearer token used to read the log.
    /// The token needs the read claim. Defaults to
    /// `$XDG_CONFIG_HOME/agentd/user.token`.
    #[arg(long, default_value_os_t = agentd_events::paths::default_user_token())]
    token_file: PathBuf,
}

#[derive(Debug, clap::Args)]
struct Decide {
    #[command(flatten)]
    connection: Connection,
    /// A file whose entire contents is the bearer token the decision is
    /// published with. The token needs the authority claim, because every
    /// request and decision type is reserved. Defaults to
    /// `$XDG_CONFIG_HOME/agentd/admin.token`.
    #[arg(long, default_value_os_t = default_authority_token())]
    authority_token_file: PathBuf,
    /// The `request_id` of the request being answered, as `pending` printed it.
    #[arg(long)]
    request_id: String,
    /// The session the request was addressed to, needed only when two requests
    /// share one `request_id` — a bridged child numbers its own requests, so its
    /// id is unique only within that child.
    #[arg(long)]
    subject: Option<String>,
    /// Allow the request.
    #[arg(long)]
    granted: bool,
    /// Refuse the request.
    #[arg(long)]
    denied: bool,
    /// Withdraw the request, as the approval deadline does.
    #[arg(long)]
    cancelled: bool,
    /// The option a bridged agent's grant selects, named as that agent offered
    /// it (`pending` prints them).
    #[arg(long)]
    option_id: Option<String>,
}

/// The default authority token file, written by `agentd init` beside the
/// daemon's `tokens.json`.
fn default_authority_token() -> PathBuf {
    agentd_events::paths::config_dir().join("admin.token")
}

/// Errors returned by the binary.
#[derive(Debug, Error)]
enum RunError {
    /// Reading a token file failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// The WebSocket connection or a frame failed.
    #[error(transparent)]
    Client(#[from] agentd_node::ClientError),
    /// The daemon did not commit or reject the decision in time.
    #[error(transparent)]
    Publish(#[from] PublishError),
    /// The daemon did not finish replaying its log in time.
    #[error("the daemon did not finish replaying its log within {0:?}")]
    Replay(Duration),
    /// No request with that id is awaiting a decision.
    #[error("no request with id {0} is awaiting a decision; run `agentd-approve pending`")]
    UnknownRequest(String),
    /// Several requests share that id, so the answer would be ambiguous.
    #[error("{1} requests have id {0}; pass --subject to say which")]
    AmbiguousRequest(String, usize),
    /// The flags did not name exactly one decision, or named none.
    #[error("pass exactly one of --granted, --denied, or --cancelled")]
    AmbiguousDecision,
    /// The request cannot be answered as asked.
    #[error(transparent)]
    Approval(#[from] agentd_node::approval::ApprovalError),
}

/// A token file's contents, trimmed of the trailing newline a file carries.
fn read_token(path: &Path) -> Result<Zeroizing<String>, RunError> {
    let token = Zeroizing::new(std::fs::read_to_string(path)?);
    Ok(Zeroizing::new(token.trim().to_string()))
}

/// Connects, replays the log from its start, and returns the events it holds.
///
/// The daemon replays ahead of the live stream and marks its end with a
/// `daemon.caught_up` notice, which is where the fold gets a stable snapshot.
async fn read_log(connection: &Connection) -> Result<Vec<Event>, RunError> {
    let token = read_token(&connection.token_file)?;
    let mut client = WsClient::connect(&connection.socket, Some(1), token.as_str()).await?;
    let mut events = Vec::new();
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let Some(wire) = tokio::time::timeout(remaining, client.next())
            .await
            .map_err(|_| RunError::Replay(TIMEOUT))??
        else {
            return Err(RunError::Replay(TIMEOUT));
        };
        if wire.event.r#type == agentd_events::DAEMON_CAUGHT_UP {
            return Ok(events);
        }
        events.push(wire.event);
    }
}

async fn run() -> Result<(), RunError> {
    match Args::parse().command {
        Command::Pending(connection) => {
            for request in pending(&read_log(&connection).await?) {
                println!("{request}");
            }
            Ok(())
        },
        Command::Decide(decide) => {
            let requests = pending(&read_log(&decide.connection).await?);
            let matching: Vec<&Pending> = requests
                .iter()
                .filter(|request| {
                    request.request_id() == decide.request_id
                        && decide
                            .subject
                            .as_deref()
                            .is_none_or(|subject| request.subject() == Some(subject))
                })
                .collect();
            let request = match matching.as_slice() {
                [] => return Err(RunError::UnknownRequest(decide.request_id)),
                [request] => *request,
                // Answering one of two would leave the other waiting under an id
                // the operator cannot tell apart.
                many => {
                    return Err(RunError::AmbiguousRequest(decide.request_id, many.len()));
                },
            };
            let decision = match (decide.granted, decide.denied, decide.cancelled) {
                (true, false, false) => Decision::Granted,
                (false, true, false) => Decision::Denied,
                (false, false, true) => Decision::Cancelled,
                _ => return Err(RunError::AmbiguousDecision),
            };
            let event = request.decide(decision, decide.option_id.as_deref())?;
            publish(&decide, &event, decision).await
        },
    }
}

/// Publishes the decision with the authority token and reports its position.
async fn publish(
    decide: &Decide,
    event: &Event,
    decision: Decision,
) -> Result<(), RunError> {
    let token = read_token(&decide.authority_token_file)?;
    let mut client = WsClient::connect(&decide.connection.socket, None, token.as_str()).await?;
    let committed = client.publish(event, TIMEOUT).await?;
    let position = committed
        .seq
        .map_or_else(|| String::from("?"), |seq| seq.to_string());
    println!(
        "decided {} request_id={} outcome={} at position {position}",
        committed.event.r#type,
        decide.request_id,
        decision.as_str()
    );
    Ok(())
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    match run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            std::process::ExitCode::FAILURE
        },
    }
}
