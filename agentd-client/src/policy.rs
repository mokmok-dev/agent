//! Which upstream connection a downstream client's event is published on.

use agentd_events::is_reserved_type;

/// Which upstream connection a client's event must be published on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// A non-reserved event, published with the user token's `publish` claim.
    User,
    /// A decision, published with the admin token's `authority` claim.
    Authority,
}

/// The only reserved types the client server forwards on the authority
/// connection: the decisions, matched exactly.
///
/// A prefix rule would also admit `*.requested`, which is what an approver
/// *reads*; a downstream client could then append a fabricated approval request
/// for the operator to answer. Every other reserved type — `error.*`,
/// `daemon.*`, `session.*` lifecycle, `sandbox.exec.*`, and the requests
/// themselves — stays refused.
const DECISIONS: &[&str] = &[
    "sandbox.permission.granted",
    "sandbox.permission.denied",
    "sandbox.permission.cancelled",
    "session.permission.granted",
    "session.permission.denied",
    "session.permission.cancelled",
    "session.egress.granted",
    "session.egress.denied",
    "session.egress.cancelled",
];

/// Classifies an event a downstream client sent.
///
/// Returns `None` when the event must be refused locally: a reserved type the
/// client server holds no authority for, or any reserved type at all while
/// approvals are disabled.
#[must_use]
pub fn route(
    r#type: &str,
    allow_approve: bool,
) -> Option<Route> {
    if !is_reserved_type(r#type) {
        return Some(Route::User);
    }
    if allow_approve && DECISIONS.contains(&r#type) {
        return Some(Route::Authority);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{Route, route};

    #[test]
    fn a_non_reserved_type_routes_to_the_user_connection() {
        assert_eq!(route("agent.inbox", false), Some(Route::User));
        assert_eq!(route("agent.inbox", true), Some(Route::User));
        assert_eq!(route("task.submitted", false), Some(Route::User));
    }

    #[test]
    fn every_decision_routes_to_the_authority_connection_only_when_enabled() {
        for decision in [
            "sandbox.permission.granted",
            "sandbox.permission.denied",
            "sandbox.permission.cancelled",
            "session.permission.granted",
            "session.permission.denied",
            "session.permission.cancelled",
            "session.egress.granted",
            "session.egress.denied",
            "session.egress.cancelled",
        ] {
            assert_eq!(route(decision, true), Some(Route::Authority), "{decision}");
            assert_eq!(route(decision, false), None, "{decision}");
        }
    }

    #[test]
    fn a_request_is_refused_even_with_approvals_enabled() {
        assert_eq!(route("sandbox.permission.requested", true), None);
        assert_eq!(route("session.permission.requested", true), None);
        assert_eq!(route("session.egress.requested", true), None);
    }

    #[test]
    fn another_reserved_family_is_refused_even_with_approvals_enabled() {
        assert_eq!(route("sandbox.exec.completed", true), None);
        assert_eq!(route("sandbox.permission", true), None);
        assert_eq!(route("session.started", true), None);
        assert_eq!(route("error.invalid_event", true), None);
        assert_eq!(route("daemon.caught_up", true), None);
    }
}
