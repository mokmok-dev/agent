//! The agent's loop: subscribe, react, publish, and acknowledge.
//!
//! One connection, the agent's own cursor, and a handler that turns a command
//! event into work. The loop is synchronous; the connection's thread does the
//! WebSocket part, and this side owns the work.
//!
//! Delivery is **at-least-once**: the bus redelivers the last acknowledged
//! sequence, so a command can arrive twice. The loop deduplicates on the event's
//! `id`, which is stable across redelivery.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use agent::client::{Client, Handler};
use agent::cloudevent::Event as CloudEvent;

use crate::contract::{Command, OUTPUT, Output};

/// What the agent needs to run.
#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// The bus's Unix socket.
    pub bus_socket: PathBuf,
    /// The session's id, which names the agent's cursor and its subject.
    pub session: String,
}

/// Everything that can go wrong while an agent runs.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The bus connection failed.
    #[error(transparent)]
    Bus(#[from] agent::client::Error),
    /// The agent could not publish its output.
    #[error("could not publish output: {0}")]
    Publish(#[from] PublishError),
}

/// Why an output could not be published.
#[derive(Debug, thiserror::Error)]
pub enum PublishError {
    /// The output event was not a valid envelope.
    #[error("the output event is not valid: {0}")]
    Envelope(#[from] agent::cloudevent::Error),
}

/// What an agent does with a command.
pub trait Capability: Send + Sync + std::fmt::Debug {
    /// Run the action, and return the output the agent publishes for it.
    fn act(
        &self,
        command: &Command,
    ) -> Output;
}

/// Run an agent for `session` until the process is stopped.
///
/// The agent subscribes as its session, reacts to every command event addressed
/// to it, publishes its outputs, and acknowledges what it processed. `start_at`
/// is the sequence to replay from, or `0` for the whole log.
///
/// # Errors
///
/// Returns an [`Error`] if the bus cannot be reached or the subscription is
/// refused.
///
/// # Panics
///
/// Never. A malformed event, or one addressed to another session, is skipped
/// and acknowledged rather than unwound, so one bad event cannot stop the loop.
pub fn run(
    config: &AgentConfig,
    capability: &dyn Capability,
    start_at: u64,
) -> Result<(), Error> {
    let client = Arc::new(Client::connect(&agent::client::Config {
        socket: config.bus_socket.clone(),
        subscriber_id: format!("agent-{}", config.session),
    })?);

    // The handler forwards to this queue, so the connection thread never does the
    // work: a slow handler stalls reads, and the bus evicts a stalled subscriber.
    let (work_send, work_recv) = mpsc::channel::<CloudEvent>();
    let send_work = Arc::new(work_send);

    let seen: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    let dedup = Arc::clone(&seen);
    let forwarder: Handler = Box::new(move |event: &CloudEvent| {
        // Dedup on the event id, which is stable across redelivery. A poisoned
        // lock fails open here, and the loop's ack records progress anyway.
        if dedup
            .lock()
            .is_ok_and(|mut seen| seen.insert(event.id.clone()))
        {
            // A queue error means the loop is already gone; dropping the event is
            // correct there, because it will be delivered again on the next run.
            let _ = send_work.send(event.clone());
        } else {
            tracing::debug!(id = %event.id, "ignoring a redelivered event");
        }
    });
    let _from = client.subscribe(start_at, forwarder)?;

    // The loop: consume one event, act, publish, acknowledge.
    for event in work_recv {
        // A command carries `data`; an event without one is malformed, and the
        // agent neither panics nor hangs on it.
        let command = event
            .subject
            .as_deref()
            .filter(|subject| *subject == config.session)
            .and(event.data.as_ref())
            .and_then(|data| Command::from_data(data).ok());
        let Some(command) = command else {
            // Not addressed to this session, or not a command: skip, but
            // acknowledge it, because the agent has seen it.
            client.ack(event.sequence.get())?;
            continue;
        };

        let output = capability.act(&command);
        publish_output(&client, &config.session, &event, &output)?;
        client.ack(event.sequence.get())?;
    }
    Ok(())
}

/// Publish one [`Output`] event for the command the agent just ran.
fn publish_output(
    client: &Client,
    session: &str,
    command_event: &CloudEvent,
    output: &Output,
) -> Result<(), Error> {
    let data = serde_json::to_value(output).map_err(|error| {
        Error::Publish(PublishError::Envelope(agent::cloudevent::Error::Json(
            error,
        )))
    })?;
    let incoming = agent::client::authored(
        OUTPUT,
        session.to_owned(),
        data,
        command_event
            .extensions
            .get("traceparent")
            .and_then(|value| value.as_str().map(str::to_owned)),
    );
    client.publish(incoming)?;
    Ok(())
}
