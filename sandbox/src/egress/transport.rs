//! Choosing the egress transport a host can actually enforce.
//!
//! Transport is a property of the **host**, decided in one place rather than by
//! the caller. The design (`docs/sandbox/network.md`) names three host states:
//!
//! | Host capability | Transport | Filesystem grant |
//! | --- | --- | --- |
//! | Can build a private network namespace | Unix socket + forwarder | the socket is bind-mounted in |
//! | No namespace (macOS Seatbelt) | loopback TCP | the profile grants the port |
//! | Linux without bubblewrap | **fails closed** | egress refused |
//!
//! The Unix-socket form makes the socket the only route, so the boundary holds.
//! The loopback form is weaker — Landlock and Seatbelt grant a **port** with no
//! host dimension, so the granted port is reachable on any address — and exists
//! only for a host that cannot do better. On Linux, which *can* express the
//! strong form, the daemon refuses egress outright when bubblewrap is missing
//! rather than downgrade to the weak form on the one host that gets it right.
//!
//! The loopback form is not selected yet: no Seatbelt renderer grants exactly
//! that port, so choosing it would grant egress without a confinement mechanism
//! behind it. Until that renderer exists, every host that cannot hold the strong
//! form fails closed, and the macOS row is a stated gap rather than a silent
//! downgrade.

use std::path::PathBuf;

use crate::filesystem::Backend;

use super::Transport;

/// What a host can do about confining egress to a single route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostCapability {
    /// The kernel can hold a private network namespace, so a Unix socket is the
    /// only route out. Linux with a usable bubblewrap.
    PrivateNetworkNamespace,
    /// No mechanism here can hold the strong form, so egress is refused rather
    /// than downgraded to a port-only grant.
    NoPrivateNetworkNamespace,
}

impl HostCapability {
    /// Classify `backend`.
    ///
    /// Only bubblewrap both confines a command and unshares the network
    /// namespace, so it is the one backend that can hold the strong form.
    #[must_use]
    pub const fn of(backend: &Backend) -> Self {
        match backend {
            Backend::Bubblewrap { .. } => Self::PrivateNetworkNamespace,
            Backend::Unsupported { .. } => Self::NoPrivateNetworkNamespace,
        }
    }
}

/// Why a transport could not be selected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TransportError {
    /// The host cannot make the proxy the only route, so egress is refused.
    ///
    /// This is the fail-closed path for Linux without bubblewrap and for macOS
    /// until a Seatbelt renderer can grant exactly the proxy's port.
    #[error("this host cannot confine egress to one route, so egress is refused")]
    Refused,
}

/// Select the transport for `capability`, using `socket` and `forward_port` for
/// the strong form.
///
/// # Errors
///
/// Returns [`TransportError::Refused`] when the host cannot hold the strong form.
/// A caller must treat that as "grant no egress", never as "run unconfined".
pub fn select_transport(
    capability: &HostCapability,
    socket: PathBuf,
    forward_port: u16,
) -> Result<Transport, TransportError> {
    match capability {
        HostCapability::PrivateNetworkNamespace => Ok(Transport::UnixSocket {
            socket,
            forward_port,
        }),
        HostCapability::NoPrivateNetworkNamespace => Err(TransportError::Refused),
    }
}

#[cfg(test)]
mod tests {
    // Tests for the capability classification and the fail-closed selection. Both
    // are pure functions of the backend, so they are covered on a host that
    // cannot spawn a namespace at all.

    use super::*;
    use crate::filesystem::BackendError;

    /// A `Backend::Bubblewrap` with a placeholder program, without probing.
    fn bubblewrap() -> Backend {
        Backend::Bubblewrap {
            program: PathBuf::from("/usr/bin/bwrap"),
        }
    }

    /// A `Backend::Unsupported`, as a host without a mechanism reports.
    fn unsupported() -> Backend {
        Backend::Unsupported {
            reason: BackendError::BubblewrapNotInstalled,
        }
    }

    #[test]
    fn bubblewrap_can_hold_a_private_network_namespace() {
        assert_eq!(
            HostCapability::of(&bubblewrap()),
            HostCapability::PrivateNetworkNamespace
        );
    }

    #[test]
    fn an_unsupported_backend_cannot_hold_a_private_network_namespace() {
        assert_eq!(
            HostCapability::of(&unsupported()),
            HostCapability::NoPrivateNetworkNamespace
        );
    }

    #[test]
    fn a_namespace_capable_host_gets_the_unix_socket_transport() {
        let transport = select_transport(
            &HostCapability::PrivateNetworkNamespace,
            PathBuf::from("/run/egress.sock"),
            8080,
        )
        .expect("selects");
        assert_eq!(
            transport,
            Transport::UnixSocket {
                socket: PathBuf::from("/run/egress.sock"),
                forward_port: 8080,
            }
        );
    }

    #[test]
    fn a_host_without_a_namespace_fails_closed() {
        assert_eq!(
            select_transport(
                &HostCapability::NoPrivateNetworkNamespace,
                PathBuf::from("/run/egress.sock"),
                8080,
            ),
            Err(TransportError::Refused)
        );
    }
}
