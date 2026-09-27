//! The authority claim: which events a connection may publish.
//!
//! `docs/sandbox/events.md` requires that a confined agent cannot approve its
//! own requests. The agent runs as the daemon's user, so `SO_PEERCRED` UID/GID
//! cannot separate it from an approver; the bus therefore gates privileged event
//! types on a *capability the connection carries*, not on peer identity.
//!
//! The capability is the **channel**: the server may listen on a second UDS
//! whose path the sandbox does not expose to the confined command. A connection
//! on that listener is [`Authority::Granted`]; a connection on the ordinary
//! listener is [`Authority::None`]. There is no token to leak, sniff, or replay,
//! and a process that never has the socket path cannot obtain the claim. This is
//! the "separate socket" option named in
//! `docs/sandbox/README.md`'s open questions.
//!
//! The distinction is a property of the *connection*, so it is enforced by the
//! server, not by [`crate::bus::Bus`]: the bus sees the same event whether or not
//! its publisher was authorized.

/// Whether a connection holds the authority claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Authority {
    /// The connection is on the ordinary listener and holds no claim.
    None,
    /// The connection is on the authority listener and may publish privileged
    /// event types.
    Granted,
}

impl Authority {
    /// Whether the connection holds the claim.
    #[must_use]
    pub const fn is_granted(self) -> bool {
        matches!(self, Self::Granted)
    }
}

/// Whether publishing an event of `ty` requires the authority claim.
///
/// The claim exists so a confined agent cannot **change decision state**:
/// approve its own command, approve its own connection, or widen its own reach.
/// The gated types are therefore the state-changing ones, across both families
/// named in `docs/sandbox/events.md`:
///
/// - the terminal decisions `granted` / `denied` / `cancelled`, and
/// - the allowlist mutations `rule_added` / `rule_revoked`.
///
/// The `requested` events are deliberately **not** gated. They are the *ask*,
/// which is the one capability the boundary grants the agent, and a decision
/// carrying an unknown `request_id` is ignored by the proxy, so a forged
/// `requested` cannot release a request the proxy did not open.
///
/// `docs/sandbox/events.md` phrases the egress rule as "any
/// `agent.sandbox.egress.*` event", broader than the decision set here. In
/// practice the only publisher of `egress.requested` is the proxy, itself an
/// authority principal, so the two readings differ only for a forger, and this
/// one keeps the ask uniform across both families. The intent stated in the same
/// section — "so an agent cannot add a rule for itself or approve its own
/// request" — is the state-changing set implemented here.
///
/// Matching is exact on the `type` string, so a lookalike such as
/// `agent.sandbox.egress.granted.x` is not accidentally privileged.
#[must_use]
pub fn requires_authority(ty: &str) -> bool {
    matches!(
        ty,
        "agent.sandbox.egress.granted"
            | "agent.sandbox.egress.denied"
            | "agent.sandbox.egress.cancelled"
            | "agent.sandbox.egress.rule_added"
            | "agent.sandbox.egress.rule_revoked"
            | "agent.sandbox.permission.granted"
            | "agent.sandbox.permission.denied"
            | "agent.sandbox.permission.cancelled"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_granted_connection_holds_the_claim() {
        assert!(!Authority::None.is_granted());
        assert!(Authority::Granted.is_granted());
    }

    #[test]
    fn the_egress_decisions_and_rule_changes_require_authority() {
        for ty in [
            "agent.sandbox.egress.granted",
            "agent.sandbox.egress.denied",
            "agent.sandbox.egress.cancelled",
            "agent.sandbox.egress.rule_added",
            "agent.sandbox.egress.rule_revoked",
        ] {
            assert!(requires_authority(ty), "{ty} must be gated");
        }
    }

    #[test]
    fn the_permission_decisions_require_authority() {
        for ty in [
            "agent.sandbox.permission.granted",
            "agent.sandbox.permission.denied",
            "agent.sandbox.permission.cancelled",
        ] {
            assert!(requires_authority(ty), "{ty} must be gated");
        }
    }

    #[test]
    fn asking_does_not_require_authority() {
        // The sandbox's only capability is to ask; `requested` is that ask, and
        // the proxy's deadline `cancelled` is recorded by the trusted daemon.
        for ty in [
            "agent.sandbox.egress.requested",
            "agent.sandbox.permission.requested",
        ] {
            assert!(!requires_authority(ty), "{ty} must not be gated");
        }
    }

    #[test]
    fn unrelated_types_do_not_require_authority() {
        for ty in [
            "agent.task.started",
            "agent.sandbox.violation.network",
            "agent.sandbox.exec.completed",
            "agent.sandbox.process.started",
        ] {
            assert!(!requires_authority(ty), "{ty} must not be gated");
        }
    }

    #[test]
    fn a_lookalike_type_is_not_privileged() {
        // Prefix matching would wrongly gate this; exact matching must not.
        for ty in [
            "agent.sandbox.egress.granted.x",
            "xagent.sandbox.egress.granted",
            "agent.sandbox.egress.granted_",
            "agent.sandbox.egress.grant",
        ] {
            assert!(!requires_authority(ty), "{ty} must not be gated");
        }
    }
}
