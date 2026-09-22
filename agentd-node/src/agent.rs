//! The agent loop: consume prompts, infer through the daemon, act, publish.
//!
//! The agent is a node specialization. It consumes the event log like any node,
//! but it *reacts*: when an [`AGENT_INBOX`](crate::conversation::AGENT_INBOX)
//! event for its conversation is applied, it runs a turn — build the
//! conversation, ask the daemon for a completion, run any tool the model calls,
//! and publish the finalized messages. Because the whole node runs inside one
//! sandbox profile, the tool process it spawns (`bash`) inherits the confinement
//! and needs no per-command permission events.
//!
//! The projection holds a non-`Sync` SQLite connection, so no reference to it
//! is held across an `await`; a turn borrows only the connection-free
//! [`Turn`] configuration.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use agentd_events::{DAEMON_CAUGHT_UP, Event, LogEntry, Seq, WireMessage};
use agentd_inference::{
    Delta, InferenceClient, InferenceRequest, Message, Role, ToolCall, ToolSpec,
};
use secrecy::{ExposeSecret as _, SecretString};
use serde::Deserialize;
use serde_json::json;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::sync::watch;
use tokio::time::timeout;

use crate::client::WsClient;
use crate::conversation::{
    AGENT_INBOX, AGENT_MESSAGE, AGENT_SESSION_STARTED, AGENT_TOOL_RESULT, AGENT_TURN_COMPLETED,
    AGENT_TURN_FAILED, AGENT_TURN_STARTED, Conversation, session_key,
};
use crate::error::AgentError;
use crate::projection::SqliteProjection;

/// The delay before the first reconnect attempt; doubled on each failure.
const INITIAL_BACKOFF: Duration = Duration::from_millis(100);

/// The longest delay between reconnect attempts.
const MAX_BACKOFF: Duration = Duration::from_secs(5);

/// How long a single inference response may take before the turn is abandoned.
///
/// Without this a stalled provider holds the agent's read loop open forever, so
/// no later prompt is ever processed.
const DEFAULT_INFERENCE_TIMEOUT: Duration = Duration::from_secs(180);

/// How many tool rounds a turn may run before it is abandoned, so a model that
/// never stops calling tools cannot spin forever.
const MAX_TOOL_ROUNDS: u32 = 64;

/// The only `CloudEvents` `type` prefix the agent applies to its projection.
const AGENT_PREFIX: &str = "agent.";

/// The instructions the model sees before the conversation.
const SYSTEM_PROMPT: &str = "\
You are a coding agent running inside a sandbox. You help with software \
engineering tasks in the workspace directory. Use the `shell` tool to inspect \
and change files and to run commands. Run one command at a time, read the \
output, and adapt. The sandbox has no network access and only the workspace is \
writable. When the task is complete, reply with a short summary and call no \
more tools.";

/// The budget a single tool command may use.
#[derive(Debug, Clone, Copy)]
pub struct ShellLimits {
    /// Wall-clock time before the command is killed.
    pub timeout: Duration,
    /// The largest output stored on the tool result; the rest is truncated.
    pub max_output_bytes: usize,
}

impl Default for ShellLimits {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(120),
            max_output_bytes: 32 * 1024,
        }
    }
}

/// A node that runs an LLM agent loop for one conversation.
#[derive(Debug)]
pub struct Agent {
    socket: PathBuf,
    source: String,
    token: SecretString,
    conversation_id: String,
    workdir: PathBuf,
    limits: ShellLimits,
    inference_timeout: Duration,
    model: Option<String>,
    projection: SqliteProjection<Conversation>,
    /// The position of the last unanswered turn already run, so recovery does
    /// not re-run the same tail twice.
    last_answered: Seq,
    /// Whether the session has been announced on the log this process.
    announced: bool,
}

impl Agent {
    /// Creates an agent for `conversation_id` that works in `workdir`.
    ///
    /// `token` accepts a `&str` or a `String`; a `&String` must be written
    /// `token.as_str()`.
    #[must_use]
    pub fn new(
        socket: impl Into<PathBuf>,
        projection: SqliteProjection<Conversation>,
        conversation_id: impl Into<String>,
        workdir: impl Into<PathBuf>,
        source: impl Into<String>,
        token: impl Into<SecretString>,
    ) -> Self {
        Self {
            socket: socket.into(),
            source: source.into(),
            token: token.into(),
            conversation_id: conversation_id.into(),
            workdir: workdir.into(),
            limits: ShellLimits::default(),
            inference_timeout: DEFAULT_INFERENCE_TIMEOUT,
            model: None,
            projection,
            last_answered: 0,
            announced: false,
        }
    }

    /// Overrides the shell tool's timeout and output cap.
    #[must_use]
    pub const fn with_limits(
        mut self,
        limits: ShellLimits,
    ) -> Self {
        self.limits = limits;
        self
    }

    /// Overrides how long a single inference response may take.
    #[must_use]
    pub const fn with_inference_timeout(
        mut self,
        timeout: Duration,
    ) -> Self {
        self.inference_timeout = timeout;
        self
    }

    /// Sets the model the agent asks the daemon for; without it the daemon uses
    /// its configured default.
    #[must_use]
    pub fn with_model(
        mut self,
        model: impl Into<String>,
    ) -> Self {
        self.model = Some(model.into());
        self
    }

    /// The agent's conversation projection.
    #[must_use]
    pub const fn projection(&self) -> &SqliteProjection<Conversation> {
        &self.projection
    }

    /// Runs until `shutdown` becomes `true`, reconnecting with backoff while the
    /// daemon is unavailable.
    ///
    /// # Errors
    ///
    /// Returns [`AgentError`] if the projection fails or a turn cannot publish
    /// its events. A connection failure is not fatal: the agent reconnects.
    pub async fn run(
        &mut self,
        mut shutdown: watch::Receiver<bool>,
    ) -> Result<(), AgentError> {
        let mut backoff = INITIAL_BACKOFF;
        loop {
            if *shutdown.borrow() {
                return Ok(());
            }

            let from = self.projection.applied_seq().saturating_add(1);
            match WsClient::connect(&self.socket, Some(from), self.token.expose_secret()).await {
                Ok(mut client) => {
                    let received = self.session(&mut client, &mut shutdown).await?;
                    if received {
                        backoff = INITIAL_BACKOFF;
                    }
                    if *shutdown.borrow() {
                        return Ok(());
                    }
                },
                Err(error) => {
                    tracing::warn!(
                        %error,
                        socket = %self.socket.display(),
                        "the agent failed to connect to the daemon",
                    );
                },
            }

            tokio::select! {
                () = tokio::time::sleep(backoff) => {},
                _ = shutdown.changed() => return Ok(()),
            }
            backoff = backoff.saturating_mul(2).min(MAX_BACKOFF);
        }
    }

    /// Processes one connection until it closes or `shutdown` fires.
    async fn session(
        &mut self,
        client: &mut WsClient,
        shutdown: &mut watch::Receiver<bool>,
    ) -> Result<bool, AgentError> {
        if !self.announced {
            publish(
                client,
                AGENT_SESSION_STARTED,
                json!({
                    "conversation_id": self.conversation_id,
                    "workdir": session_key(&self.workdir),
                    "model": self.model.as_deref().unwrap_or(""),
                }),
            )
            .await?;
            self.announced = true;
        }
        let mut received = false;
        loop {
            tokio::select! {
                _ = shutdown.changed() => return Ok(received),
                message = client.next() => {
                    match message {
                        Ok(Some(wire)) => {
                            received = true;
                            if self.handle(wire)?.is_some() {
                                let mut history = Conversation::history(
                                    self.projection.connection(),
                                    &self.conversation_id,
                                )?;
                                let turn = Turn {
                                    socket: &self.socket,
                                    token: &self.token,
                                    workdir: &self.workdir,
                                    limits: self.limits,
                                    inference_timeout: self.inference_timeout,
                                    source: &self.source,
                                    conversation_id: &self.conversation_id,
                                    model: self.model.as_deref(),
                                };
                                if let Err(error) = turn.run(client, &mut history).await {
                                    tracing::debug!(%error, "the agent turn failed");
                                }                            }
                        },
                        Ok(None) => return Ok(received),
                        Err(error) => {
                            tracing::warn!(%error, "the agent connection failed");
                            return Ok(received);
                        },
                    }
                },
            }
        }
    }

    /// Applies or skips one message, returning the tail position when a turn
    /// should start.
    ///
    /// A turn starts when the conversation's last message is unanswered — a user
    /// prompt, a tool result, or an assistant turn still awaiting its tool
    /// results — and it has not already been run. That makes a turn interrupted
    /// by a restart resume: the daemon's `daemon.caught_up` notice triggers the
    /// check once the replay has been applied, and a live inbox triggers it when
    /// it arrives. An inbox for another conversation is applied but not answered.
    fn handle(
        &mut self,
        wire: WireMessage,
    ) -> Result<Option<Seq>, AgentError> {
        let Some(seq) = wire.seq else {
            if wire.event.r#type == DAEMON_CAUGHT_UP {
                return self.pending_turn();
            }
            return Ok(None);
        };
        if seq <= self.projection.applied_seq() {
            return Ok(None);
        }
        if wire.event.r#type == AGENT_INBOX
            && let Some(conversation) = wire
                .event
                .data
                .get("conversation_id")
                .and_then(serde_json::Value::as_str)
            && conversation != self.conversation_id
        {
            tracing::info!(
                conversation,
                served = %self.conversation_id,
                "an inbox for another conversation was applied but not answered",
            );
        }
        if wire.event.r#type.starts_with(AGENT_PREFIX) {
            self.projection.apply(LogEntry::new(seq, wire.event))?;
        } else {
            self.projection.skip(seq)?;
        }
        self.pending_turn()
    }

    /// Returns the tail position when the conversation has an unanswered turn.
    fn pending_turn(&mut self) -> Result<Option<Seq>, AgentError> {
        let Some((seq, message)) =
            Conversation::tail(self.projection.connection(), &self.conversation_id)?
        else {
            return Ok(None);
        };
        let pending = match message.role {
            Role::User | Role::Tool => true,
            Role::Assistant => !message.tool_calls.is_empty(),
            Role::System => false,
        };
        if pending && seq > self.last_answered {
            self.last_answered = seq;
            return Ok(Some(seq));
        }
        Ok(None)
    }
}

/// The connection-free configuration a turn borrows.
///
/// Every field is `Sync`, so a future holding `&Turn` is `Send` — unlike a
/// future holding the agent, whose projection is not `Sync`.
struct Turn<'a> {
    socket: &'a Path,
    token: &'a SecretString,
    workdir: &'a Path,
    limits: ShellLimits,
    inference_timeout: Duration,
    source: &'a str,
    conversation_id: &'a str,
    model: Option<&'a str>,
}

impl Turn<'_> {
    /// Runs one turn, publishing its lifecycle around the inference/tool loop.
    async fn run(
        &self,
        client: &mut WsClient,
        history: &mut Vec<Message>,
    ) -> Result<(), AgentError> {
        use tracing::Instrument as _;
        let span = tracing::info_span!(
            "agent.turn",
            conversation_id = %self.conversation_id,
            agent_id = %self.source,
        );
        let result = self.run_inner(client, history).instrument(span).await;
        match &result {
            Ok(()) => tracing::info!("the agent turn completed"),
            Err(error) => tracing::warn!(%error, "the agent turn failed"),
        }
        result
    }

    /// The turn body, run inside the turn's span.
    async fn run_inner(
        &self,
        client: &mut WsClient,
        history: &mut Vec<Message>,
    ) -> Result<(), AgentError> {
        publish(
            client,
            AGENT_TURN_STARTED,
            json!({ "conversation_id": self.conversation_id, "agent_id": self.source }),
        )
        .await?;

        let result = self.drive(client, history).await;

        match &result {
            Ok(()) => {
                publish(
                    client,
                    AGENT_TURN_COMPLETED,
                    json!({ "conversation_id": self.conversation_id }),
                )
                .await?;
            },
            Err(error) => {
                let _ = publish(
                    client,
                    AGENT_TURN_FAILED,
                    json!({
                        "conversation_id": self.conversation_id,
                        "error": error.to_string(),
                    }),
                )
                .await;
            },
        }
        result
    }

    /// The inference/tool loop, until the model stops calling tools.
    async fn drive(
        &self,
        client: &mut WsClient,
        history: &mut Vec<Message>,
    ) -> Result<(), AgentError> {
        let tools = vec![shell_tool()];
        let mut inference =
            InferenceClient::connect(self.socket, self.token.expose_secret()).await?;
        let mut rounds = 0;
        loop {
            rounds += 1;
            if rounds > MAX_TOOL_ROUNDS {
                return Err(AgentError::TooManyToolRounds(MAX_TOOL_ROUNDS));
            }
            let request = InferenceRequest {
                messages: request_messages(history),
                tools: tools.clone(),
                model: self.model.map(String::from),
            };
            tracing::debug!(
                round = rounds,
                messages = request.messages.len(),
                "requesting inference"
            );
            let Ok(deltas) = timeout(self.inference_timeout, inference.complete(&request)).await
            else {
                return Err(AgentError::InferenceTimeout(self.inference_timeout));
            };
            let deltas = deltas?;
            tracing::debug!(round = rounds, "inference responded");
            let (text, calls) = fold_deltas(deltas)?;

            if text.is_empty() && calls.is_empty() {
                return Ok(());
            }
            let has_calls = !calls.is_empty();
            let message = if has_calls {
                Message::with_tool_calls(text, calls)
            } else {
                Message::assistant(text)
            };
            publish(
                client,
                AGENT_MESSAGE,
                message_data(self.conversation_id, &message),
            )
            .await?;

            let mut results = Vec::with_capacity(message.tool_calls.len());
            for call in &message.tool_calls {
                tracing::debug!(tool = %call.name, "running a tool");
                let outcome = run_tool(call, self.workdir, self.limits).await;
                tracing::debug!(tool = %call.name, timed_out = outcome.timed_out, "the tool finished");
                publish(
                    client,
                    AGENT_TOOL_RESULT,
                    tool_result_data(self.conversation_id, call, &outcome),
                )
                .await?;
                results.push(Message::tool(call.id.clone(), outcome.content));
            }
            history.push(message);
            history.extend(results);

            if !has_calls {
                return Ok(());
            }
        }
    }
}

/// The arguments of the `shell` tool.
#[derive(Debug, Deserialize)]
struct ShellArguments {
    command: String,
}

/// The result of one tool command.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ToolOutcome {
    content: String,
    exit_code: Option<i32>,
    timed_out: bool,
}

/// The model's tool description for the shell.
fn shell_tool() -> ToolSpec {
    ToolSpec {
        name: String::from("shell"),
        description: String::from(
            "Run a bash command in the workspace directory and return its combined \
             stdout and stderr and exit code.",
        ),
        parameters: json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The bash command line to run.",
                },
            },
            "required": ["command"],
        }),
    }
}

/// Builds the request messages: the system instruction followed by `history`.
fn request_messages(history: &[Message]) -> Vec<Message> {
    let mut messages = Vec::with_capacity(history.len() + 1);
    messages.push(Message::system(SYSTEM_PROMPT));
    messages.extend_from_slice(history);
    messages
}

/// Folds a response stream into the accumulated text and complete tool calls.
///
/// # Errors
///
/// Returns [`AgentError::Provider`] if the stream ends with a [`Delta::Error`].
fn fold_deltas(deltas: Vec<Delta>) -> Result<(String, Vec<ToolCall>), AgentError> {
    let mut text = String::new();
    let mut calls = Vec::new();
    for delta in deltas {
        match delta {
            Delta::Text { text: fragment } => text.push_str(&fragment),
            Delta::ToolCall {
                id,
                name,
                arguments,
            } => calls.push(ToolCall {
                id,
                name,
                arguments,
            }),
            Delta::Error { message } => return Err(AgentError::Provider(message)),
            Delta::Done { .. } => {},
        }
    }
    Ok((text, calls))
}

/// The event data for a finalized assistant message.
fn message_data(
    conversation_id: &str,
    message: &Message,
) -> serde_json::Value {
    json!({
        "conversation_id": conversation_id,
        "content": message.content,
        "tool_calls": message.tool_calls,
    })
}

/// The event data for a tool result.
fn tool_result_data(
    conversation_id: &str,
    call: &ToolCall,
    outcome: &ToolOutcome,
) -> serde_json::Value {
    json!({
        "conversation_id": conversation_id,
        "tool_call_id": call.id,
        "content": outcome.content,
        "exit_code": outcome.exit_code,
        "timed_out": outcome.timed_out,
    })
}

/// Publishes a `CloudEvents` message to the daemon.
///
/// The event carries the current span's trace context, when tracing is active,
/// so its downstream consumers can correlate with the turn it belongs to.
async fn publish(
    client: &mut WsClient,
    r#type: &str,
    data: serde_json::Value,
) -> Result<(), AgentError> {
    tracing::debug!(r#type, "publishing an event");
    let mut event = Event::new(r#type, data);
    if let Some(traceparent) = agentd_telemetry::semconv::current_traceparent() {
        event = event.with_traceparent(&traceparent);
    }
    client.send(&event).await?;
    tracing::debug!(r#type, "the event was sent");
    Ok(())
}

/// Runs one tool call.
async fn run_tool(
    call: &ToolCall,
    workdir: &Path,
    limits: ShellLimits,
) -> ToolOutcome {
    let arguments = match serde_json::from_str::<ShellArguments>(&call.arguments) {
        Ok(arguments) => arguments,
        Err(error) => {
            return ToolOutcome {
                content: format!("invalid tool arguments: {error}"),
                exit_code: None,
                timed_out: false,
            };
        },
    };
    run_shell(&arguments.command, workdir, limits).await
}

/// Runs `command` with `bash -c` in `workdir`, enforcing the limits.
///
/// Output is read with a hard cap so a runaway command cannot exhaust memory:
/// once either stream reaches the cap the child is killed and the output is
/// marked truncated.
async fn run_shell(
    command: &str,
    workdir: &Path,
    limits: ShellLimits,
) -> ToolOutcome {
    let mut child = match Command::new("bash")
        .arg("-c")
        .arg(command)
        .current_dir(workdir)
        .kill_on_drop(true)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            return ToolOutcome {
                content: format!("failed to run the command: {error}"),
                exit_code: None,
                timed_out: false,
            };
        },
    };
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let cap = limits.max_output_bytes;

    let completed = timeout(limits.timeout, async move {
        let (stdout, stderr) = tokio::join!(read_capped(stdout, cap), read_capped(stderr, cap));
        let (stdout, stdout_capped) = stdout;
        let (stderr, stderr_capped) = stderr;
        let capped = stdout_capped || stderr_capped;
        let exit_code = if capped {
            let _ = child.kill().await;
            None
        } else {
            child.wait().await.ok().and_then(|status| status.code())
        };
        (stdout, stderr, exit_code, capped)
    })
    .await;

    let Ok((stdout, stderr, exit_code, capped)) = completed else {
        let seconds = limits.timeout.as_secs();
        return ToolOutcome {
            content: format!("timed out after {seconds}s"),
            exit_code: None,
            timed_out: true,
        };
    };
    let mut combined = String::from_utf8_lossy(&stdout).into_owned();
    let stderr = String::from_utf8_lossy(&stderr);
    if !stderr.is_empty() {
        if !combined.is_empty() {
            combined.push('\n');
        }
        combined.push_str(&stderr);
    }
    if capped {
        combined.push_str("\n[output truncated]");
    }
    let status = exit_code.map_or_else(|| String::from("signal"), |code| code.to_string());
    ToolOutcome {
        content: format!("exit code: {status}\n{combined}"),
        exit_code,
        timed_out: false,
    }
}

/// Reads a child stream up to `cap` bytes, reporting whether the cap was hit.
///
/// One extra byte is read so a stream exactly at the cap is not reported as
/// truncated.
async fn read_capped(
    stream: Option<impl AsyncRead + Unpin>,
    cap: usize,
) -> (Vec<u8>, bool) {
    let Some(stream) = stream else {
        return (Vec::new(), false);
    };
    let mut reader = stream.take((cap as u64).saturating_add(1));
    let mut buffer = Vec::new();
    if reader.read_to_end(&mut buffer).await.is_err() {
        return (buffer, true);
    }
    if buffer.len() > cap {
        buffer.truncate(cap);
        return (buffer, true);
    }
    (buffer, false)
}

#[cfg(test)]
mod tests {
    use super::{Agent, ShellLimits, fold_deltas, request_messages, run_shell};
    use crate::conversation::Conversation;
    use crate::error::AgentError;
    use crate::projection::SqliteProjection;
    use agentd_events::{DAEMON_CAUGHT_UP, Event};
    use agentd_inference::{Delta, Message, ToolCall};
    use serde_json::json;
    use std::time::Duration;

    fn agent() -> (tempfile::TempDir, Agent) {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let projection = SqliteProjection::<Conversation>::open(dir.path().join("node.db"))
            .expect("projection should open");
        let agent = Agent::new(
            "/tmp/agentd-agent-test.sock",
            projection,
            "c1",
            dir.path(),
            "urn:test:agent",
            "token",
        );
        (dir, agent)
    }

    #[test]
    fn request_messages_prepends_the_system_prompt() {
        let messages = request_messages(&[Message::user("hello")]);

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, agentd_inference::Role::System);
        assert_eq!(messages[1], Message::user("hello"));
    }

    #[test]
    fn fold_deltas_accumulates_text_and_tool_calls() {
        let (text, calls) = fold_deltas(vec![
            Delta::Text {
                text: String::from("I will run "),
            },
            Delta::Text {
                text: String::from("ls."),
            },
            Delta::ToolCall {
                id: String::from("call-1"),
                name: String::from("shell"),
                arguments: String::from(r#"{"command":"ls"}"#),
            },
            Delta::Done {
                finish_reason: Some(String::from("tool_calls")),
            },
        ])
        .expect("folding should succeed");

        assert_eq!(text, "I will run ls.");
        assert_eq!(
            calls,
            vec![ToolCall {
                id: String::from("call-1"),
                name: String::from("shell"),
                arguments: String::from(r#"{"command":"ls"}"#),
            }]
        );
    }

    #[test]
    fn fold_deltas_surfaces_a_provider_error() {
        let error = fold_deltas(vec![Delta::Error {
            message: String::from("boom"),
        }])
        .expect_err("a provider error should surface");

        assert!(matches!(error, AgentError::Provider(message) if message == "boom"));
    }

    #[tokio::test]
    async fn run_shell_captures_output_and_exit_codes() {
        let dir = tempfile::tempdir().expect("tempdir should be created");

        let ok = run_shell("echo hello", dir.path(), ShellLimits::default()).await;
        assert_eq!(ok.exit_code, Some(0));
        assert!(ok.content.contains("hello"));
        assert!(!ok.timed_out);

        let bad = run_shell("exit 3", dir.path(), ShellLimits::default()).await;
        assert_eq!(bad.exit_code, Some(3));
    }

    #[tokio::test]
    async fn run_shell_bounds_its_output() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let limits = ShellLimits {
            max_output_bytes: 64,
            ..ShellLimits::default()
        };

        let outcome = run_shell("yes hello | head -c 100000", dir.path(), limits).await;

        assert!(outcome.content.ends_with("[output truncated]"));
        assert!(outcome.content.len() < 200);
    }

    #[tokio::test]
    async fn run_shell_times_out() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let limits = ShellLimits {
            timeout: Duration::from_millis(100),
            ..ShellLimits::default()
        };

        let outcome = run_shell("sleep 5", dir.path(), limits).await;

        assert!(outcome.timed_out);
        assert_eq!(outcome.exit_code, None);
    }

    #[test]
    fn an_inbox_for_another_conversation_does_not_trigger() {
        let (_dir, mut agent) = agent();
        let wire = agentd_events::WireMessage {
            seq: Some(1),
            event: Event::new(
                crate::conversation::AGENT_INBOX,
                json!({ "conversation_id": "other", "content": "hi" }),
            ),
        };

        assert!(
            agent.handle(wire).expect("handle should succeed").is_none(),
            "an inbox for another conversation must not start a turn"
        );
        assert_eq!(agent.projection().applied_seq(), 1);
    }

    #[test]
    fn an_inbox_for_this_conversation_triggers() {
        let (_dir, mut agent) = agent();
        let wire = agentd_events::WireMessage {
            seq: Some(1),
            event: Event::new(
                crate::conversation::AGENT_INBOX,
                json!({ "conversation_id": "c1", "content": "hi" }),
            ),
        };

        assert_eq!(agent.handle(wire).expect("handle should succeed"), Some(1));
        assert_eq!(agent.projection().applied_seq(), 1);
    }

    #[test]
    fn a_non_agent_event_advances_the_checkpoint_without_triggering() {
        let (_dir, mut agent) = agent();
        let wire = agentd_events::WireMessage {
            seq: Some(7),
            event: Event::new("task.submitted", json!({})),
        };

        assert!(agent.handle(wire).expect("handle should succeed").is_none());
        assert_eq!(agent.projection().applied_seq(), 7);
    }

    #[test]
    fn a_caught_up_notice_triggers_a_pending_turn_after_a_restart() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let db = dir.path().join("node.db");
        // One agent applies an inbox but never runs its turn.
        {
            let projection =
                SqliteProjection::<Conversation>::open(&db).expect("projection should open");
            let mut first =
                Agent::new("/tmp/a.sock", projection, "c1", dir.path(), "urn:test", "t");
            let _ = first
                .handle(agentd_events::WireMessage {
                    seq: Some(1),
                    event: Event::new(
                        crate::conversation::AGENT_INBOX,
                        json!({ "conversation_id": "c1", "content": "unanswered" }),
                    ),
                })
                .expect("handle should succeed");
        }
        // A restarted agent recovers it on the caught-up notice.
        let projection =
            SqliteProjection::<Conversation>::open(&db).expect("projection should reopen");
        let mut agent = Agent::new("/tmp/a.sock", projection, "c1", dir.path(), "urn:test", "t");
        let trigger = agent
            .handle(agentd_events::WireMessage {
                seq: None,
                event: Event::new(DAEMON_CAUGHT_UP, json!({})),
            })
            .expect("handle should succeed");

        assert_eq!(trigger, Some(1));
    }

    #[test]
    fn an_already_run_tail_is_not_triggered_again() {
        let (_dir, mut agent) = agent();
        let _ = agent
            .handle(agentd_events::WireMessage {
                seq: Some(1),
                event: Event::new(
                    crate::conversation::AGENT_INBOX,
                    json!({ "conversation_id": "c1", "content": "hi" }),
                ),
            })
            .expect("handle should succeed");

        let trigger = agent
            .handle(agentd_events::WireMessage {
                seq: None,
                event: Event::new(DAEMON_CAUGHT_UP, json!({})),
            })
            .expect("handle should succeed");

        assert_eq!(trigger, None);
    }

    #[test]
    fn a_generated_shell_tool_is_well_formed() {
        let tool = super::shell_tool();
        assert_eq!(tool.name, "shell");
        assert_eq!(tool.parameters["required"], json!(["command"]));
    }
}
