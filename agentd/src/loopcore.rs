//! The agent's loop: subscribe, react, publish, and acknowledge.
//!
//! One connection, the agent's own cursor, and a handler that turns a command
//! event into work. The loop is synchronous; the connection's thread does the
//! WebSocket part, and this side owns the work.
//!
//! A command is one call to a [`Capability`], and the output that call returns is
//! the command's result. A capability that takes a while may also report as it
//! works, and the loop publishes each report as a `progress` output, so the log
//! shows the work as it happens.
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
use serde_json::Value;

use crate::contract::{Command, OUTPUT, Output, OutputKind};

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
    ///
    /// The returned output is the command's result, and the loop publishes it.
    /// While the action runs it may report through `reporter`, whose reports the
    /// loop publishes as `progress` outputs. That is what makes an action that
    /// takes minutes visible while it runs instead of only when it ends. A
    /// capability that reports nothing is ordinary.
    fn act(
        &self,
        command: &Command,
        reporter: &dyn Reporter,
    ) -> Output;
}

/// Where a capability reports what it is doing while it works.
///
/// The loop supplies one per command, already named for that command's action,
/// so a report carries only the detail. Reporting is **best effort**: a report
/// that cannot be published is logged and dropped. That is deliberate, because
/// the returned [`Output`] is the command's result and it is published over the
/// same connection, so a dead bus is reported by that publish instead. A
/// capability therefore cannot fail a command by reporting, and one that reports
/// nothing at all is ordinary.
pub trait Reporter {
    /// Publish one progress report for the command being run.
    fn report(
        &self,
        detail: Value,
    );
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

        let output = capability.act(
            &command,
            &BusReporter {
                client: &client,
                session: &config.session,
                command_event: &event,
                action: &command.action,
            },
        );
        publish_output(&client, &config.session, &event, &output)?;
        client.ack(event.sequence.get())?;
    }
    Ok(())
}

/// The loop's reporter: publishes each report as a `progress` output.
///
/// It is built per command, so a report needs only the detail: the action, the
/// session, and the trace context come from the command being run.
#[derive(Debug)]
struct BusReporter<'a> {
    /// The connection the reports are published on.
    client: &'a Client,
    /// The session whose subject the reports are published under.
    session: &'a str,
    /// The command the reports belong to, for its trace context.
    command_event: &'a CloudEvent,
    /// The action the reports are about, echoed from the command.
    action: &'a str,
}

impl Reporter for BusReporter<'_> {
    fn report(
        &self,
        detail: Value,
    ) {
        let output = Output {
            kind: OutputKind::Progress,
            action: self.action.to_owned(),
            detail,
        };
        if let Err(error) = publish_output(self.client, self.session, self.command_event, &output) {
            tracing::warn!(%error, "could not publish progress");
        }
    }
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
