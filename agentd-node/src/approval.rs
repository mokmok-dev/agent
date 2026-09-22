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
use std::collections::BTreeSet;
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
const REQUEST_TYPES: &[(&str, RequestKind)] = &[
    ("sandbox.permission.requested", RequestKind::Sandbox),
    ("session.permission.requested", RequestKind::Session),
    ("session.egress.requested", RequestKind::Egress),
];

/// The decision types, which answer a request carrying the same `request_id`.
const DECISION_TYPES: &[&str] = &[
    "sandbox.permission.granted",
    "sandbox.permission.denied",
    "session.permission.decided",
    "session.egress.granted",
    "session.egress.denied",
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
        REQUEST_TYPES
            .iter()
            .find(|(candidate, _)| *candidate == r#type)
            .map(|(_, kind)| *kind)
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
    #[error("pass --option-id: a bridged agent's grant names the option it selects")]
    MissingOption,
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

impl Pending {
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

    /// The event that answers this request, addressed to the same session.
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
                    return Err(ApprovalError::MissingOption);
                };
                if !offered.contains(&option_id) {
                    return Err(ApprovalError::UnknownOption(
                        option_id.to_string(),
                        offered.join(", "),
                    ));
                }
                Event::new(
                    "session.permission.decided",
                    json!({ "request_id": self.request_id, "option_id": option_id }),
                )
            },
            (RequestKind::Session, Decision::Denied | Decision::Cancelled) => Event::new(
                "session.permission.decided",
                json!({ "request_id": self.request_id, "cancelled": true }),
            ),
            (RequestKind::Egress, Decision::Granted) => Event::new(
                "session.egress.granted",
                self.egress_data("granted by an approver"),
            ),
            (RequestKind::Egress, Decision::Denied | Decision::Cancelled) => {
                let reason = match decision {
                    Decision::Denied => "denied by an approver",
                    _ => "cancelled by an approver",
                };
                Event::new("session.egress.denied", self.egress_data(reason))
            },
            (RequestKind::Sandbox, decision) => {
                let (r#type, value) = match decision {
                    Decision::Granted => ("sandbox.permission.granted", "granted"),
                    Decision::Denied => ("sandbox.permission.denied", "denied"),
                    Decision::Cancelled => ("sandbox.permission.denied", "cancelled"),
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

    /// The ids of the options a bridged agent offered.
    fn option_ids(&self) -> Vec<&str> {
        self.data
            .get("options")
            .and_then(Value::as_array)
            .map(|options| {
                options
                    .iter()
                    .filter_map(|option| option.get("optionId").and_then(Value::as_str))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The proxy's decision payload, echoing the destination it asked about.
    fn egress_data(
        &self,
        reason: &str,
    ) -> Value {
        json!({
            "request_id": self.request_id,
            "host": self.data.get("host").cloned().unwrap_or(Value::Null),
            "port": self.data.get("port").cloned().unwrap_or(Value::Null),
            "reason": reason,
        })
    }

    /// What the request is about, for the operator deciding it.
    fn detail(&self) -> String {
        match self.kind {
            RequestKind::Sandbox => format!(
                "command={}",
                self.data.get("command").and_then(Value::as_str).unwrap_or("")
            ),
            RequestKind::Session => {
                let options = self.option_ids();
                if options.is_empty() {
                    String::from("options=-")
                } else {
                    format!("options={}", options.join(","))
                }
            },
            RequestKind::Egress => format!(
                "destination={}:{}",
                self.data.get("host").and_then(Value::as_str).unwrap_or(""),
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
/// gives the requests awaiting a decision as of its end.
#[must_use]
pub fn pending<'a>(events: impl IntoIterator<Item = &'a Event>) -> Vec<Pending> {
    let mut waiting: Vec<Pending> = Vec::new();
    // A sandbox request that a static rule already decided is recorded with a
    // decision of its own and is not waiting for anyone.
    let mut decided: BTreeSet<String> = BTreeSet::new();
    for event in events {
        let Some(request_id) = event.data.get("request_id").and_then(Value::as_str) else {
            continue;
        };
        if DECISION_TYPES.contains(&event.r#type.as_str()) {
            decided.insert(request_id.to_string());
            waiting.retain(|pending| pending.request_id != request_id);
            continue;
        }
        let Some(kind) = RequestKind::requested_by(&event.r#type) else {
            continue;
        };
        if event
            .data
            .get("decision")
            .and_then(Value::as_str)
            .is_some_and(|decision| decision != DECISION_PENDING)
        {
            continue;
        }
        if decided.contains(request_id) {
            continue;
        }
        waiting.push(Pending {
            kind,
            request_id: request_id.to_string(),
            subject: event.subject.clone(),
            data: event.data.clone(),
        });
    }
    waiting
}

#[cfg(test)]
mod tests {
    use super::{ApprovalError, Decision, RequestKind, pending};
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
            "session.permission.decided",
            json!({ "request_id": "5", "cancelled": true }),
        );
        let unanswered = session_request("6");

        let waiting = pending([&asked, &answered, &unanswered]);
        assert_eq!(waiting.len(), 1);
        assert_eq!(waiting[0].request_id(), "6");
        assert_eq!(waiting[0].kind(), RequestKind::Session);
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
        assert_eq!(waiting[0].to_string(), "sandbox.permission.requested request_id=1 subject=- command=rm -rf /");
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
        assert_eq!(granted.r#type, "session.permission.decided");
        assert_eq!(granted.data["request_id"], "5");
        assert_eq!(granted.data["option_id"], "allow-once");
        assert_eq!(granted.subject.as_deref(), Some("session:agent"));

        let missing = pending.decide(Decision::Granted, None);
        assert!(matches!(missing, Err(ApprovalError::MissingOption)));

        let unknown = pending.decide(Decision::Granted, Some("allow-always"));
        assert!(matches!(unknown, Err(ApprovalError::UnknownOption(_, _))));

        // Cancelling needs no option: the agent is told the outcome is cancelled.
        let cancelled = pending.decide(Decision::Cancelled, None).expect("cancel");
        assert_eq!(cancelled.data["cancelled"], true);
        assert_eq!(cancelled.data.get("option_id"), None);
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
