//! The approver's view of the approval contract: which recorded requests are
//! still awaiting a decision, and which event answers each one.
//!
//! Three producers ask for a decision and all correlate it by `request_id`: the
//! sandbox's `sandbox.permission.requested` (a confined command), a bridged
//! agent's `session.permission.requested` (a tool call, addressed to one session
//! by `subject`), and the CONNECT proxy's `session.egress.requested` (a host).
//! The type strings are restated here rather than imported because the node
//! links neither the sandbox nor the daemon; the `agentd-approve` integration
//! test drives the real daemon, so a drift from a producer fails there.

use agentd_events::Event;
use serde_json::{Value, json};
use thiserror::Error;

/// The kind of request awaiting a decision, which fixes what answers it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RequestKind {
    /// `sandbox.permission.requested`: a confined command awaits a decision.
    Sandbox,
    /// `session.permission.requested`: a bridged agent asks to run a tool call.
    Session,
    /// `session.egress.requested`: a session asks to reach a host.
    Egress,
}

/// The three request types, in the order they are printed.
const REQUEST_KINDS: [RequestKind; 3] = [
    RequestKind::Sandbox,
    RequestKind::Session,
    RequestKind::Egress,
];

/// An approver allows a bridged agent's request, selecting one of the options
/// the agent offered.
const SESSION_PERMISSION_GRANTED: &str = "session.permission.granted";
/// An approver refuses a bridged agent's request, selecting a rejecting option
/// when the agent offered one.
const SESSION_PERMISSION_DENIED: &str = "session.permission.denied";
/// Nobody decided a bridged agent's request, so it is withdrawn.
const SESSION_PERMISSION_CANCELLED: &str = "session.permission.cancelled";
/// An approver allows a destination outside the session's static allowlist.
const EGRESS_GRANTED: &str = "session.egress.granted";
/// An approver refuses that destination.
const EGRESS_DENIED: &str = "session.egress.denied";
/// Nobody decided that destination, so it is withdrawn.
const EGRESS_CANCELLED: &str = "session.egress.cancelled";

/// The decision types, which answer a request carrying the same `request_id`.
///
/// The sandbox's types are spelled out here because this crate must not depend
/// on the daemon binary that produces them; a drift fails
/// `agentd/tests/approve.rs`, which drives the real daemon.
const DECISION_TYPES: &[&str] = &[
    "sandbox.permission.granted",
    "sandbox.permission.denied",
    "sandbox.permission.cancelled",
    SESSION_PERMISSION_GRANTED,
    SESSION_PERMISSION_DENIED,
    SESSION_PERMISSION_CANCELLED,
    EGRESS_GRANTED,
    EGRESS_DENIED,
    EGRESS_CANCELLED,
];

/// The `decision` value on a sandbox request that waits for an approver; an
/// `auto` one was already decided by a static policy rule.
const DECISION_PENDING: &str = "pending";

impl RequestKind {
    /// The event type that carries a request of this kind.
    #[must_use]
    pub const fn requested_type(self) -> &'static str {
        match self {
            Self::Sandbox => "sandbox.permission.requested",
            Self::Session => "session.permission.requested",
            Self::Egress => "session.egress.requested",
        }
    }

    /// The kind an event `type` requests, if any.
    #[must_use]
    fn requested_by(r#type: &str) -> Option<Self> {
        REQUEST_KINDS
            .into_iter()
            .find(|kind| kind.requested_type() == r#type)
    }
}

/// The decision an approver reaches.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Decision {
    /// Allow the request.
    Granted,
    /// Refuse it.
    Denied,
    /// Withdraw it, as the approval deadline does.
    Cancelled,
}

impl Decision {
    /// The decision's name, as a report prints it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Granted => "granted",
            Self::Denied => "denied",
            Self::Cancelled => "cancelled",
        }
    }
}

/// A decision an approver cannot express.
#[derive(Debug, Error)]
pub enum ApprovalError {
    /// A bridged agent picks among the options it offered, so a grant without
    /// one has no meaning to send.
    #[error("a grant for a bridged agent must name one of the options it offered: {0}")]
    MissingOption(String),
    /// The named option is not one the agent offered.
    #[error("{0} is not one of the options the agent offered: {1}")]
    UnknownOption(String, String),
}

/// A request no approver has answered yet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pending {
    kind: RequestKind,
    request_id: String,
    /// The `subject` the request carried, which routes an answer to the session
    /// that asked. Absent for a sandbox or egress request, which correlate by
    /// `request_id` alone.
    subject: Option<String>,
    data: Value,
}

/// What a request is identified by.
///
/// A bridged child numbers its own requests, so its `request_id` is unique only
/// within that child: the session a request is addressed to is part of its
/// identity, or two sessions asking under one id would share an answer.
type RequestKey = (Option<String>, String);

/// A value as it is safe to print: a command, a host, and an option id all come
/// from an agent or a session, so an escape sequence in one must not reach the
/// operator's terminal raw.
fn printable(value: &str) -> String {
    value.escape_debug().to_string()
}

/// The key an event is correlated by.
fn key_of(event: &Event) -> Option<RequestKey> {
    Some((
        event.subject.clone(),
        event
            .data
            .get("request_id")
            .and_then(Value::as_str)?
            .to_string(),
    ))
}

impl Pending {
    /// What this request is identified by.
    fn key(&self) -> RequestKey {
        (self.subject.clone(), self.request_id.clone())
    }

    /// The request's kind, which fixes what answers it.
    #[must_use]
    pub const fn kind(&self) -> RequestKind {
        self.kind
    }

    /// The id a decision is correlated by.
    #[must_use]
    pub fn request_id(&self) -> &str {
        &self.request_id
    }

    /// The session the request came from, when it is addressed to one.
    #[must_use]
    pub fn subject(&self) -> Option<&str> {
        self.subject.as_deref()
    }

    /// The request's own payload, as the producer published it: the command a
    /// sandbox asked about, the tool call and options a bridged agent offered,
    /// or the destination a session wants to reach.
    #[must_use]
    pub const fn data(&self) -> &Value {
        &self.data
    }

    /// The event that answers this request, addressed to the same session.
    ///
    /// A grant for a bridged agent must name one of the options that agent
    /// offered; a denial selects the agent's own reject option when it offered
    /// one, and otherwise cancels the request, which is all the protocol can
    /// express.
    ///
    /// # Errors
    ///
    /// Returns [`ApprovalError::MissingOption`] or [`ApprovalError::UnknownOption`]
    /// when a bridged agent's grant does not name one of the options that agent
    /// offered.
    pub fn decide(
        &self,
        decision: Decision,
        option_id: Option<&str>,
    ) -> Result<Event, ApprovalError> {
        let event = match (self.kind, decision) {
            (RequestKind::Session, Decision::Granted) => {
                let offered = self.option_ids();
                let Some(option_id) = option_id else {
                    return Err(ApprovalError::MissingOption(offered.join(", ")));
                };
                if !offered.contains(&option_id) {
                    return Err(ApprovalError::UnknownOption(
                        option_id.to_string(),
                        offered.join(", "),
                    ));
                }
                Self::answer(
                    SESSION_PERMISSION_GRANTED,
                    json!({
                        "request_id": self.request_id,
                        "decision": "granted",
                        "option_id": option_id,
                    }),
                )
            },
            (RequestKind::Session, Decision::Denied) => self.reject_option().map_or_else(
                || self.cancellation(),
                |option_id| {
                    Self::answer(
                        SESSION_PERMISSION_DENIED,
                        json!({
                            "request_id": self.request_id,
                            "decision": "denied",
                            "option_id": option_id,
                        }),
                    )
                },
            ),
            (RequestKind::Session, Decision::Cancelled) => self.cancellation(),
            (RequestKind::Egress, Decision::Granted) => Event::new(
                EGRESS_GRANTED,
                self.egress_data("granted", "granted by an approver"),
            ),
            (RequestKind::Egress, Decision::Denied) => Event::new(
                EGRESS_DENIED,
                self.egress_data("denied", "denied by an approver"),
            ),
            (RequestKind::Egress, Decision::Cancelled) => Event::new(
                EGRESS_CANCELLED,
                self.egress_data("cancelled", "cancelled by an approver"),
            ),
            (RequestKind::Sandbox, decision) => {
                let (r#type, value) = match decision {
                    Decision::Granted => ("sandbox.permission.granted", "granted"),
                    Decision::Denied => ("sandbox.permission.denied", "denied"),
                    Decision::Cancelled => ("sandbox.permission.cancelled", "cancelled"),
                };
                let mut data = self.data.clone();
                data["decision"] = json!(value);
                Event::new(r#type, data)
            },
        };
        Ok(match &self.subject {
            Some(subject) => event.with_subject(subject),
            None => event,
        })
    }

    /// The event that answers a bridged agent's request.
    fn answer(
        r#type: &str,
        data: Value,
    ) -> Event {
        Event::new(r#type, data)
    }

    /// The `session.permission.cancelled` that withdraws a request.
    fn cancellation(&self) -> Event {
        Self::answer(
            SESSION_PERMISSION_CANCELLED,
            json!({ "request_id": self.request_id, "decision": "cancelled" }),
        )
    }

    /// The options a bridged agent offered for the request.
    fn options(&self) -> &[Value] {
        self.data
            .get("options")
            .and_then(Value::as_array)
            .map_or(&[], Vec::as_slice)
    }

    /// The ids of the options a bridged agent offered.
    fn option_ids(&self) -> Vec<&str> {
        self.options()
            .iter()
            .filter_map(|option| option.get("optionId").and_then(Value::as_str))
            .collect()
    }

    /// The id of the option a bridged agent offered for refusing, when it
    /// offered one. ACP names those `reject_once` and `reject_always`.
    fn reject_option(&self) -> Option<&str> {
        self.options()
            .iter()
            .find(|option| {
                option
                    .get("kind")
                    .and_then(Value::as_str)
                    .is_some_and(|kind| kind.starts_with("reject"))
            })
            .and_then(|option| option.get("optionId"))
            .and_then(Value::as_str)
    }

    /// The proxy's decision payload, echoing the destination it asked about.
    fn egress_data(
        &self,
        decision: &str,
        reason: &str,
    ) -> Value {
        json!({
            "request_id": self.request_id,
            "decision": decision,
            "host": self.data.get("host").cloned().unwrap_or(Value::Null),
            "port": self.data.get("port").cloned().unwrap_or(Value::Null),
            "reason": reason,
        })
    }

    /// What the request is about, for the operator deciding it.
    ///
    /// Every value here is chosen by the agent or the session rather than by the
    /// operator, so each is escaped: an escape sequence in a command must not
    /// rewrite the output the operator reads it from.
    fn detail(&self) -> String {
        match self.kind {
            RequestKind::Sandbox => format!(
                "command={}",
                printable(
                    self.data
                        .get("command")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                )
            ),
            RequestKind::Session => {
                let options = self.option_ids();
                if options.is_empty() {
                    String::from("options=-")
                } else {
                    format!(
                        "options={}",
                        options
                            .iter()
                            .map(|option| printable(option))
                            .collect::<Vec<_>>()
                            .join(",")
                    )
                }
            },
            RequestKind::Egress => format!(
                "destination={}:{}",
                printable(self.data.get("host").and_then(Value::as_str).unwrap_or("")),
                self.data
                    .get("port")
                    .map_or_else(|| String::from("?"), Value::to_string),
            ),
        }
    }
}

impl std::fmt::Display for Pending {
    fn fmt(
        &self,
        formatter: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        write!(
            formatter,
            "{} request_id={} subject={} {}",
            self.kind.requested_type(),
            self.request_id,
            self.subject.as_deref().unwrap_or("-"),
            self.detail()
        )
    }
}

/// Collects the requests in `events` that no decision answers, in log order.
///
/// A request answered later in the log is not pending, so folding the whole log
/// gives the requests awaiting a decision as of its end. A decision answers only
/// the request it is addressed to: a bridged child numbers its own requests, so
/// an id alone is not unique. Nothing is remembered about a decision once it has
/// been folded: a request that appears *after* one is a fresh ask — a restarted
/// child numbers its requests from the start again — and must still be listed.
#[must_use]
pub fn pending<'a>(events: impl IntoIterator<Item = &'a Event>) -> Vec<Pending> {
    let mut waiting: Vec<Pending> = Vec::new();
    for event in events {
        let Some(key) = key_of(event) else {
            continue;
        };
        if DECISION_TYPES.contains(&event.r#type.as_str()) {
            waiting.retain(|pending| pending.key() != key);
            continue;
        }
        let Some(kind) = RequestKind::requested_by(&event.r#type) else {
            continue;
        };
        // A sandbox request that a static rule already decided is recorded with
        // a decision of its own and is not waiting for anyone.
        if event
            .data
            .get("decision")
            .and_then(Value::as_str)
            .is_some_and(|decision| decision != DECISION_PENDING)
        {
            continue;
        }
        waiting.push(Pending {
            kind,
            request_id: key.1,
            subject: key.0,
            data: event.data.clone(),
        });
    }
    waiting
}

#[cfg(test)]
mod tests {
    use super::{ApprovalError, Decision, RequestKind, SESSION_PERMISSION_CANCELLED, pending};
    use agentd_events::Event;
    use serde_json::json;

    fn session_request(request_id: &str) -> Event {
        Event::new(
            "session.permission.requested",
            json!({
                "protocol": "acp",
                "session_id": "agent",
                "request_id": request_id,
                "tool_call": { "toolCallId": "call_1" },
                "options": [
                    { "optionId": "allow-once", "name": "Allow", "kind": "allow_once" },
                    { "optionId": "reject-once", "name": "Reject", "kind": "reject_once" },
                ],
            }),
        )
        .with_subject("session:agent")
    }

    #[test]
    fn a_request_with_no_decision_is_pending_and_a_decided_one_is_not() {
        let asked = session_request("5");
        let answered = Event::new(
            SESSION_PERMISSION_CANCELLED,
            json!({ "request_id": "5", "cancelled": true }),
        )
        .with_subject("session:agent");
        let unanswered = session_request("6");

        let waiting = pending([&asked, &answered, &unanswered]);
        assert_eq!(waiting.len(), 1);
        assert_eq!(waiting[0].request_id(), "6");
        assert_eq!(waiting[0].kind(), RequestKind::Session);
    }

    #[test]
    fn a_decision_answers_only_the_session_it_is_addressed_to() {
        // A bridged child numbers its own requests, so two sessions can ask
        // under one id; only the subject tells the answers apart.
        let first = session_request("7");
        let second = session_request("7").with_subject("session:other");
        let answer = Event::new(
            SESSION_PERMISSION_CANCELLED,
            json!({ "request_id": "7", "cancelled": true }),
        )
        .with_subject("session:agent");

        let waiting = pending([&first, &second, &answer]);
        assert_eq!(waiting.len(), 1);
        assert_eq!(waiting[0].subject(), Some("session:other"));
        assert_eq!(waiting[0].request_id(), "7");
    }

    #[test]
    fn the_three_request_types_are_recognised_and_others_are_not() {
        let sandbox = Event::new(
            "sandbox.permission.requested",
            json!({
                "sandbox_id": "s1",
                "request_id": "1",
                "command": "rm -rf /",
                "decision": "pending",
            }),
        );
        let egress = Event::new(
            "session.egress.requested",
            json!({ "request_id": "2", "host": "example.com", "port": 443 }),
        );
        let unrelated = Event::new("agent.message", json!({ "request_id": "3" }));

        let waiting = pending([&sandbox, &egress, &unrelated]);
        assert_eq!(waiting.len(), 2);
        assert_eq!(waiting[0].kind(), RequestKind::Sandbox);
        assert_eq!(
            waiting[0].to_string(),
            "sandbox.permission.requested request_id=1 subject=- command=rm -rf /"
        );
        assert_eq!(waiting[1].kind(), RequestKind::Egress);
        assert_eq!(
            waiting[1].to_string(),
            "session.egress.requested request_id=2 subject=- destination=example.com:443"
        );
    }

    #[test]
    fn a_request_a_static_rule_already_decided_is_not_pending() {
        // The sandbox records an automatically allowed command with the
        // decision on the request itself.
        let auto = Event::new(
            "sandbox.permission.requested",
            json!({ "request_id": "1", "command": "ls", "decision": "auto" }),
        );

        assert!(pending([&auto]).is_empty());
    }

    #[test]
    fn a_granted_session_permission_names_one_of_the_offered_options() {
        let waiting = pending([&session_request("5")]);
        let pending = &waiting[0];

        let granted = pending
            .decide(Decision::Granted, Some("allow-once"))
            .expect("an offered option");
        assert_eq!(granted.r#type, "session.permission.granted");
        assert_eq!(granted.data["request_id"], "5");
        assert_eq!(granted.data["option_id"], "allow-once");
        assert_eq!(granted.subject.as_deref(), Some("session:agent"));

        let missing = pending.decide(Decision::Granted, None);
        assert!(
            matches!(missing, Err(ApprovalError::MissingOption(ref offered)) if offered == "allow-once, reject-once")
        );

        let unknown = pending.decide(Decision::Granted, Some("allow-always"));
        assert!(matches!(unknown, Err(ApprovalError::UnknownOption(_, _))));

        // Cancelling needs no option: the agent is told the outcome is cancelled.
        let cancelled = pending.decide(Decision::Cancelled, None).expect("cancel");
        assert_eq!(cancelled.r#type, "session.permission.cancelled");
        assert_eq!(cancelled.data["decision"], "cancelled");
        assert_eq!(cancelled.data.get("option_id"), None);
    }

    #[test]
    fn a_denied_session_permission_selects_the_agents_own_reject_option() {
        // The fixture's agent offers `reject-once`, so denying names it and the
        // agent hears which outcome the operator chose.
        let asked = pending([&session_request("5")]);
        let denied = asked[0].decide(Decision::Denied, None).expect("deny");
        assert_eq!(denied.r#type, "session.permission.denied");
        assert_eq!(denied.data["option_id"], "reject-once");
        assert_eq!(denied.data.get("cancelled"), None);

        // An agent that offers only an allow option can only be cancelled.
        let allow_only = Event::new(
            "session.permission.requested",
            json!({
                "request_id": "6",
                "options": [{ "optionId": "allow-once", "kind": "allow_once" }],
            }),
        )
        .with_subject("session:agent");
        let asked = pending([&allow_only]);
        let denied = asked[0].decide(Decision::Denied, None).expect("deny");
        assert_eq!(denied.r#type, "session.permission.cancelled");
        assert_eq!(denied.data["decision"], "cancelled");
        assert_eq!(denied.data.get("option_id"), None);
    }

    #[test]
    fn a_request_asked_again_after_a_decision_is_pending_once_more() {
        // A supervised session that restarts numbers its requests from the start
        // again, so the same id under the same subject asks a second time; the
        // first answer must not hide it.
        let first = session_request("1");
        let answered = Event::new(
            SESSION_PERMISSION_CANCELLED,
            json!({ "request_id": "1", "cancelled": true }),
        )
        .with_subject("session:agent");
        let again = session_request("1");

        let waiting = pending([&first, &answered, &again]);
        assert_eq!(waiting.len(), 1);
        assert_eq!(waiting[0].request_id(), "1");
        assert_eq!(waiting[0].subject(), Some("session:agent"));
    }

    #[test]
    fn a_withdrawn_sandbox_request_is_recorded_as_cancelled() {
        // The sandbox understands granted, denied, and cancelled, so a
        // withdrawal is its own outcome rather than a denial the operator never
        // made.
        let asked = Event::new(
            "sandbox.permission.requested",
            json!({ "request_id": "4", "command": "rm -rf /tmp/x", "decision": "pending" }),
        );
        let asked = pending([&asked]);
        let withdrawn = asked[0].decide(Decision::Cancelled, None).expect("cancel");

        assert_eq!(withdrawn.r#type, "sandbox.permission.cancelled");
        assert_eq!(withdrawn.data["decision"], "cancelled");
        assert_eq!(withdrawn.data["command"], "rm -rf /tmp/x");
    }

    #[test]
    fn a_printed_request_escapes_what_the_session_chose() {
        // The command and the destination are not the operator's, so a control
        // sequence in one must not reach the terminal.
        let asked = Event::new(
            "sandbox.permission.requested",
            json!({ "request_id": "1", "command": "echo \u{1b}[2Jcleared", "decision": "pending" }),
        );
        let printed = pending([&asked])[0].to_string();

        assert!(!printed.contains('\u{1b}'), "{printed}");
        assert!(printed.contains("\\u{1b}"), "{printed}");
    }

    #[test]
    fn an_egress_decision_echoes_the_destination_it_answers() {
        let asked = Event::new(
            "session.egress.requested",
            json!({ "request_id": "9", "host": "api.example.com", "port": 443 }),
        );
        let pending = &pending([&asked])[0];

        let granted = pending.decide(Decision::Granted, None).expect("grant");
        assert_eq!(granted.r#type, "session.egress.granted");
        assert_eq!(granted.data["host"], "api.example.com");
        assert_eq!(granted.data["port"], 443);

        let denied = pending.decide(Decision::Denied, None).expect("deny");
        assert_eq!(denied.r#type, "session.egress.denied");
        assert_eq!(denied.data["reason"], "denied by an approver");
    }

    #[test]
    fn a_sandbox_decision_carries_the_command_it_answers() {
        let asked = Event::new(
            "sandbox.permission.requested",
            json!({
                "sandbox_id": "s1",
                "request_id": "4",
                "agent_id": "urn:test:agent",
                "resource": "shell",
                "action": "exec",
                "command": "echo hi",
                "decision": "pending",
            }),
        );
        let pending = &pending([&asked])[0];

        let granted = pending.decide(Decision::Granted, None).expect("grant");
        assert_eq!(granted.r#type, "sandbox.permission.granted");
        assert_eq!(granted.data["decision"], "granted");
        assert_eq!(granted.data["sandbox_id"], "s1");
        assert_eq!(granted.data["command"], "echo hi");

        let denied = pending.decide(Decision::Denied, None).expect("deny");
        assert_eq!(denied.r#type, "sandbox.permission.denied");
        assert_eq!(denied.data["decision"], "denied");
    }

    #[test]
    fn a_request_without_a_request_id_is_ignored() {
        let nameless = Event::new(
            "session.egress.requested",
            json!({ "host": "example.com", "port": 80 }),
        );

        assert!(pending([&nameless]).is_empty());
    }

    /// The fold must not confuse a decision's own `request_id` with a request:
    /// a decision for an id that was never asked does not become a request.
    #[test]
    fn a_decision_for_an_unseen_request_does_not_create_one() {
        let decision = Event::new(
            "session.egress.denied",
            json!({ "request_id": "11", "host": "example.com", "port": 80, "reason": "x" }),
        );

        assert!(pending([&decision]).is_empty());
    }
}
