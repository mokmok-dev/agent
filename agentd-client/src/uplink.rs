//! The publish path: one task owns the daemon connections and answers each
//! downstream publish with a verdict correlated by the client's event id.

use crate::frame::{PUBLISH_COMMITTED, PUBLISH_FAILED, notice};
use crate::policy::Route;
use agentd_events::{Event, Seq};
use agentd_node::{PublishError, WsClient};
use axum::extract::ws::Message;
use secrecy::{ExposeSecret as _, SecretString};
use serde_json::json;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, watch};

/// How many publish requests may queue before downstream clients are
/// backpressured.
const UPLINK_CAPACITY: usize = 64;

/// The most terminal verdicts remembered for one run.
///
/// The bound is what keeps a client that publishes at machine rate from growing
/// the map without limit; evicting the oldest only costs a re-attempt.
const VERDICT_HISTORY: usize = 1024;

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
    /// The daemon committed the event at this position. Terminal.
    Committed(Seq),
    /// The daemon refused the append. Terminal: the same id would be refused
    /// again, so it is remembered.
    Rejected(String),
    /// The daemon was not reached, or did not answer in time. Not terminal: the
    /// event may still have committed, so a retry is attempted again rather
    /// than answered from a stale failure.
    Unknown(String),
}

/// The daemon socket, the credentials, and the verdicts of this run.
#[derive(Debug)]
pub struct Uplink {
    socket: PathBuf,
    user_token: Arc<SecretString>,
    admin_token: Option<Arc<SecretString>>,
    timeout: Duration,
    /// The remembered verdicts, and the order they were first recorded in, so
    /// the oldest can be evicted at [`VERDICT_HISTORY`].
    verdicts: HashMap<String, Verdict>,
    recorded: VecDeque<String>,
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
            recorded: VecDeque::new(),
        }
    }

    /// Publishes `request` and returns the verdict notice for it.
    ///
    /// A repeated id is answered from a recorded terminal verdict, so a retry
    /// cannot append the event twice within one run. An unresolved publish is
    /// not recorded: the caller must resolve it from the log, and answering it
    /// with a stale failure would block recovery after the daemon returns.
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
        if !matches!(verdict, Verdict::Unknown(_)) {
            self.remember(id.to_owned(), verdict);
        }
        notice
    }

    /// Records a terminal verdict, evicting the oldest past
    /// [`VERDICT_HISTORY`].
    fn remember(
        &mut self,
        id: String,
        verdict: Verdict,
    ) {
        self.verdicts.insert(id.clone(), verdict);
        self.recorded.push_back(id);
        while self.recorded.len() > VERDICT_HISTORY {
            if let Some(oldest) = self.recorded.pop_front() {
                self.verdicts.remove(&oldest);
            }
        }
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
                None => return Verdict::Unknown(String::from("approvals are not configured")),
            },
        };
        let mut client = match WsClient::connect(&self.socket, None, token.expose_secret()).await {
            Ok(client) => client,
            Err(error) => {
                return Verdict::Unknown(format!("connecting to the daemon failed: {error}"));
            },
        };
        match client.publish(&request.event, self.timeout).await {
            Ok(envelope) => envelope.seq.map_or_else(
                || {
                    Verdict::Unknown(String::from(
                        "the daemon committed the event without a position",
                    ))
                },
                Verdict::Committed,
            ),
            Err(PublishError::Rejected(reason)) => Verdict::Rejected(reason),
            Err(PublishError::Timeout(_)) => Verdict::Unknown(String::from(
                "the daemon did not answer in time; the event may have committed, so \
                 resolve it from the log",
            )),
            Err(PublishError::Client(error)) => {
                Verdict::Unknown(format!("the publish failed: {error}"))
            },
        }
    }
}

/// Renders a verdict as its correlated downstream notice.
///
/// `outcome` tells the client whether the id is finished with: `rejected` is
/// terminal, while `unknown` must be resolved from the log before a resend.
fn render(
    id: &str,
    verdict: &Verdict,
) -> Message {
    match verdict {
        Verdict::Committed(seq) => notice(PUBLISH_COMMITTED, json!({ "event_id": id, "seq": seq })),
        Verdict::Rejected(error) => notice(
            PUBLISH_FAILED,
            json!({ "event_id": id, "outcome": "rejected", "error": error }),
        ),
        Verdict::Unknown(error) => notice(
            PUBLISH_FAILED,
            json!({ "event_id": id, "outcome": "unknown", "error": error }),
        ),
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
                // The verdict is offered, never awaited: waiting on one client's
                // bounded queue would park this single task, and with it every
                // other client's publishing, behind a client that stopped
                // reading. A dropped verdict leaves the client to resolve the id
                // from its own replay, which is the documented retry rule.
                if request.reply.try_send(verdict).is_err() {
                    tracing::debug!("a verdict was dropped because its client is behind");
                }
            },
        }
    }
}
