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

/// The decision families the client server forwards on the authority
/// connection.
///
/// Every other reserved family — `error.*`, `daemon.*`, `session.started`,
/// `sandbox.exec.completed` — stays refused, so a downstream client cannot use
/// the client server to append a reserved event it has no decision for.
const DECISION_PREFIXES: &[&str] = &[
    "sandbox.permission.",
    "session.permission.",
    "session.egress.",
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
    if allow_approve
        && DECISION_PREFIXES
            .iter()
            .any(|prefix| r#type.starts_with(prefix))
    {
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
    fn a_decision_routes_to_the_authority_connection_only_when_enabled() {
        assert_eq!(
            route("sandbox.permission.granted", true),
            Some(Route::Authority)
        );
        assert_eq!(
            route("session.permission.cancelled", true),
            Some(Route::Authority)
        );
        assert_eq!(route("session.egress.denied", true), Some(Route::Authority));
        assert_eq!(route("sandbox.permission.granted", false), None);
    }

    #[test]
    fn another_reserved_family_is_refused_even_with_approvals_enabled() {
        assert_eq!(route("sandbox.exec.completed", true), None);
        assert_eq!(route("session.started", true), None);
        assert_eq!(route("error.invalid_event", true), None);
        assert_eq!(route("daemon.caught_up", true), None);
    }
}
