//! The publish path: one task owns the daemon connections and answers each
//! downstream publish with a verdict correlated by the client's event id.

use crate::frame::{PUBLISH_COMMITTED, PUBLISH_FAILED, notice};
use crate::policy::Route;
use agentd_events::{Event, Seq};
use agentd_node::{PublishError, WsClient};
use axum::extract::ws::Message;
use secrecy::{ExposeSecret as _, SecretString};
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, watch};

/// How many publish requests may queue before downstream clients are
/// backpressured.
const UPLINK_CAPACITY: usize = 64;

/// A publish a downstream client asked for.
#[derive(Debug)]
pub struct PublishRequest {
    /// The event exactly as the client sent it, with its client-chosen id.
    pub event: Event,
    /// Which upstream connection to publish on.
    pub route: Route,
    /// Where the correlated verdict notice is sent.
    pub reply: mpsc::Sender<Message>,
}

/// The channel downstream handlers use to reach the publish task.
#[must_use]
pub fn channel() -> (mpsc::Sender<PublishRequest>, mpsc::Receiver<PublishRequest>) {
    mpsc::channel(UPLINK_CAPACITY)
}

/// The terminal outcome of one publish, remembered so a retry with the same id
/// is answered without a second append.
#[derive(Debug, Clone)]
enum Verdict {
    /// The daemon committed the event at this position.
    Committed(Seq),
    /// The publish did not commit. The text is the daemon's reason, or the
    /// client server's when the daemon could not be reached.
    Failed(String),
}

/// The daemon socket, the credentials, and the verdicts of this run.
#[derive(Debug)]
pub struct Uplink {
    socket: PathBuf,
    user_token: Arc<SecretString>,
    admin_token: Option<Arc<SecretString>>,
    timeout: Duration,
    verdicts: HashMap<String, Verdict>,
}

impl Uplink {
    /// Creates an uplink publishing on `socket` with the given bearer tokens.
    #[must_use]
    pub fn new(
        socket: PathBuf,
        user_token: Arc<SecretString>,
        admin_token: Option<Arc<SecretString>>,
        timeout: Duration,
    ) -> Self {
        Self {
            socket,
            user_token,
            admin_token,
            timeout,
            verdicts: HashMap::new(),
        }
    }

    /// Publishes `request` and returns the verdict notice for it.
    ///
    /// A repeated id is answered from the recorded verdict, so a retry cannot
    /// append the event twice within one run.
    async fn handle(
        &mut self,
        request: &PublishRequest,
    ) -> Message {
        let id = request.event.id.as_str();
        if let Some(verdict) = self.verdicts.get(id) {
            return render(id, verdict);
        }
        let verdict = self.publish(request).await;
        let notice = render(id, &verdict);
        self.verdicts.insert(id.to_owned(), verdict);
        notice
    }

    /// Connects, publishes, and classifies the outcome.
    ///
    /// A connection is opened per request and dropped after it: an idle
    /// connection stays subscribed to the live stream, and the `error.lagged`
    /// it eventually receives would be read as a rejection of the next publish.
    async fn publish(
        &self,
        request: &PublishRequest,
    ) -> Verdict {
        let token = match request.route {
            Route::User => Arc::clone(&self.user_token),
            Route::Authority => match &self.admin_token {
                Some(token) => Arc::clone(token),
                None => return Verdict::Failed(String::from("approvals are not configured")),
            },
        };
        let mut client = match WsClient::connect(&self.socket, None, token.expose_secret()).await {
            Ok(client) => client,
            Err(error) => {
                return Verdict::Failed(format!("connecting to the daemon failed: {error}"));
            },
        };
        match client.publish(&request.event, self.timeout).await {
            Ok(envelope) => envelope.seq.map_or_else(
                || {
                    Verdict::Failed(String::from(
                        "the daemon committed the event without a position",
                    ))
                },
                Verdict::Committed,
            ),
            Err(PublishError::Rejected(reason)) => Verdict::Failed(reason),
            Err(PublishError::Timeout(_)) => Verdict::Failed(String::from(
                "the daemon did not answer in time; the event may have committed, so \
                 resolve it from the log",
            )),
            Err(PublishError::Client(error)) => {
                Verdict::Failed(format!("the publish failed: {error}"))
            },
        }
    }
}

/// Renders a verdict as its correlated downstream notice.
fn render(
    id: &str,
    verdict: &Verdict,
) -> Message {
    match verdict {
        Verdict::Committed(seq) => notice(PUBLISH_COMMITTED, json!({ "event_id": id, "seq": seq })),
        Verdict::Failed(error) => notice(PUBLISH_FAILED, json!({ "event_id": id, "error": error })),
    }
}

/// Serves publish requests until `shutdown` becomes `true` or every sender is
/// dropped.
pub async fn run(
    mut uplink: Uplink,
    mut requests: mpsc::Receiver<PublishRequest>,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return;
                }
            },
            request = requests.recv() => {
                let Some(request) = request else {
                    return;
                };
                let verdict = uplink.handle(&request).await;
                // The client may have disconnected while its publish was in
                // flight; a verdict with nowhere to go is not an error.
                let _ = request.reply.send(verdict).await;
            },
        }
    }
}
