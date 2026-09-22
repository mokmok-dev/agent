//! A minimal client that publishes one event to the daemon and waits for it to
//! be committed.
//!
//! It exists so the system can be driven from a shell. With `--inbox` it sends
//! a user prompt to the current session; otherwise it publishes a raw event.
//! It connects to the event API over the Unix domain socket, sends the event,
//! and reports the position the daemon assigned (or the error notice it
//! answered with).

use agentd_events::Event;
use agentd_node::{AGENT_INBOX, Conversation, SqliteProjection, WsClient, session_key};
use clap::Parser;
use secrecy::zeroize::Zeroizing;
use serde_json::{Value, json};
use std::borrow::Cow;
use std::path::PathBuf;
use std::time::Duration;
use thiserror::Error;

/// How long to wait for the daemon to commit or reject the event.
const TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Parser)]
#[command(name = "agentd-publish", version = env!("CARGO_PKG_VERSION"))]
struct Args {
    /// The daemon's event WebSocket Unix socket.
    #[arg(long, default_value_os_t = agentd_events::paths::default_socket())]
    socket: PathBuf,
    /// A file whose entire contents is the bearer token the daemon expects. The
    /// token needs the publish claim (and read, to observe the commit). Defaults
    /// to `$XDG_CONFIG_HOME/agentd/user.token`.
    #[arg(long, default_value_os_t = agentd_events::paths::default_user_token())]
    token_file: PathBuf,
    /// The SQLite projection used to resolve the session when `--conversation`
    /// is omitted. Defaults to `$XDG_DATA_HOME/agentd/agent.db`.
    #[arg(long, default_value_os_t = default_db())]
    db: PathBuf,
    /// The workspace whose session to target. Defaults to the current directory.
    #[arg(long, default_value_os_t = default_workdir())]
    workdir: PathBuf,
    /// The conversation to target. By default the most recent session recorded
    /// for `--workdir`.
    #[arg(long)]
    conversation: Option<String>,
    /// Send a user prompt to the session: shorthand for
    /// `--type agent.inbox --data '{"conversation_id":...,"content":...}'`.
    #[arg(long)]
    inbox: Option<String>,
    /// The `CloudEvents` `type` (required without `--inbox`).
    #[arg(long = "type")]
    r#type: Option<String>,
    /// The event `data` as JSON (required without `--inbox`).
    #[arg(long)]
    data: Option<String>,
}

/// The default workspace: the process's current directory.
fn default_workdir() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// The default projection path.
fn default_db() -> PathBuf {
    agentd_events::paths::data_dir().join("agent.db")
}

/// Errors returned by the binary.
#[derive(Debug, Error)]
enum RunError {
    /// Reading the token file failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// The `--data` argument was not valid JSON.
    #[error("the --data value is not valid JSON: {0}")]
    Data(#[from] serde_json::Error),
    /// Opening the projection to resolve a session failed.
    #[error(transparent)]
    Projection(agentd_node::AgentError),
    /// The WebSocket connection failed.
    #[error(transparent)]
    Client(#[from] agentd_node::ClientError),
    /// The daemon did not commit or reject the event in time.
    #[error(transparent)]
    Publish(#[from] agentd_node::PublishError),
    /// `--inbox` was given but no session exists for the workdir.
    #[error("no session found for this workdir; start agentd-agent first")]
    NoSession,
    /// Neither `--inbox` nor `--type`/`--data` was given.
    #[error("pass --inbox <text>, or both --type and --data")]
    MissingEvent,
}

/// Resolves the event to publish: an inbox shorthand or a raw event.
fn resolve_event(args: &Args) -> Result<(&str, Value), RunError> {
    if let Some(content) = &args.inbox {
        let conversation: Cow<'_, str> = if let Some(conversation) = &args.conversation {
            Cow::Borrowed(conversation)
        } else {
            let projection =
                SqliteProjection::<Conversation>::open(&args.db).map_err(RunError::Projection)?;
            let key = session_key(&args.workdir);
            Cow::Owned(
                Conversation::latest_session(projection.connection(), &key)
                    .map_err(RunError::Projection)?
                    .ok_or(RunError::NoSession)?,
            )
        };
        return Ok((
            AGENT_INBOX,
            json!({ "conversation_id": conversation, "content": content }),
        ));
    }
    match (&args.r#type, &args.data) {
        (Some(r#type), Some(data)) => Ok((r#type.as_str(), serde_json::from_str(data)?)),
        _ => Err(RunError::MissingEvent),
    }
}

async fn run() -> Result<(), RunError> {
    let args = Args::parse();
    let (r#type, data) = resolve_event(&args)?;
    let token = Zeroizing::new(std::fs::read_to_string(&args.token_file)?);
    let event = Event::new(r#type, data);
    let mut client = WsClient::connect(&args.socket, None, token.trim()).await?;
    let committed = client.publish(&event, TIMEOUT).await?;
    let position = committed
        .seq
        .map_or_else(|| String::from("?"), |seq| seq.to_string());
    println!("committed {} at position {position}", committed.event.r#type);
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
