//! Peer access control for the transport.
//!
//! Per `docs/event-bus/security.md`, access is the UDS pathname permissions plus
//! a `SO_PEERCRED` UID/GID allowlist. This module holds the credential type and
//! the allowlist; the socket permissions live in the parent module.

/// The credentials of the process on the other end of a Unix socket.
///
/// Obtained from `SO_PEERCRED` on Linux and the platform equivalent elsewhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerCredential {
    /// The peer's user ID.
    pub uid: u32,
    /// The peer's group ID.
    pub gid: u32,
    /// The peer's process ID, when the platform reports it.
    pub pid: Option<u32>,
}

/// A UID/GID allowlist for incoming connections.
///
/// A peer is permitted when its UID is listed, or its GID is listed. An empty
/// allowlist permits nobody, which is the safe default: access must be granted
/// deliberately.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Allowlist {
    uids: Vec<u32>,
    gids: Vec<u32>,
}

impl Allowlist {
    /// An allowlist that permits nobody.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            uids: Vec::new(),
            gids: Vec::new(),
        }
    }

    /// Permit `uid`, returning the updated allowlist.
    #[must_use]
    pub fn allow_uid(
        mut self,
        uid: u32,
    ) -> Self {
        self.uids.push(uid);
        self
    }

    /// Permit `gid`, returning the updated allowlist.
    #[must_use]
    pub fn allow_gid(
        mut self,
        gid: u32,
    ) -> Self {
        self.gids.push(gid);
        self
    }

    /// Whether the allowlist permits nobody.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.uids.is_empty() && self.gids.is_empty()
    }

    /// Whether `credential` is permitted.
    ///
    /// A match on either the UID or the GID is enough.
    #[must_use]
    pub fn permits(
        &self,
        credential: &PeerCredential,
    ) -> bool {
        self.uids.contains(&credential.uid) || self.gids.contains(&credential.gid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALICE: PeerCredential = PeerCredential {
        uid: 1000,
        gid: 1000,
        pid: Some(42),
    };
    const GROUP_MEMBER: PeerCredential = PeerCredential {
        uid: 2000,
        gid: 100,
        pid: Some(43),
    };
    const STRANGER: PeerCredential = PeerCredential {
        uid: 3000,
        gid: 3000,
        pid: None,
    };

    #[test]
    fn an_empty_allowlist_permits_nobody() {
        let allowlist = Allowlist::new();
        assert!(allowlist.is_empty());
        assert!(!allowlist.permits(&ALICE));
    }

    #[test]
    fn a_listed_uid_is_permitted() {
        let allowlist = Allowlist::new().allow_uid(1000);
        assert!(!allowlist.is_empty());
        assert!(allowlist.permits(&ALICE));
        assert!(!allowlist.permits(&STRANGER));
    }

    #[test]
    fn a_listed_gid_is_permitted() {
        let allowlist = Allowlist::new().allow_gid(100);
        assert!(allowlist.permits(&GROUP_MEMBER));
        assert!(!allowlist.permits(&ALICE));
    }

    #[test]
    fn a_credential_matches_on_either_uid_or_gid() {
        let allowlist = Allowlist::new().allow_uid(1000).allow_gid(100);
        assert!(allowlist.permits(&ALICE));
        assert!(allowlist.permits(&GROUP_MEMBER));
        assert!(!allowlist.permits(&STRANGER));
    }
}
