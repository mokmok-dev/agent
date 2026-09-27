//! The network domain: Unix sockets the command may reach and the egress proxy.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::{InvalidPolicy, path};

/// The command's network reach.
///
/// The zero value grants nothing: no socket, no loopback, no egress. See
/// `docs/sandbox/network.md`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkPolicy {
    /// Unix domain sockets the command may connect to, by path.
    #[serde(default)]
    pub unix_sockets: Vec<PathBuf>,
    /// Whether the command may bind its own loopback server and reach it. On
    /// Linux this lives inside the command's private network namespace.
    #[serde(default)]
    pub loopback: bool,
    /// The daemon proxy the command may reach, if egress is granted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<ProxyGrant>,
}

/// The egress proxy a command may reach.
///
/// The **presence** of a proxy is the egress grant; the `egress` set inside may
/// still be empty. An empty set reaches nothing, so the zero value of an
/// opted-in `NetworkPolicy` is "egress enabled but no destination allowed".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProxyGrant {
    /// The loopback port the command's `HTTP_PROXY` names: the forwarder on
    /// Linux, the proxy itself on macOS.
    pub port: u16,
    /// The proxy's Unix socket, when the host can cross a network namespace.
    /// `None` means the proxy is loopback TCP (macOS).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub socket: Option<PathBuf>,
    /// The destinations the proxy starts with. The zero set denies everything.
    #[serde(default)]
    pub egress: Vec<HostPort>,
}

/// An exact egress destination.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostPort {
    /// The host, compared case-insensitively. Never empty.
    pub host: String,
    /// The destination port. Never zero.
    pub port: u16,
}

impl HostPort {
    /// A destination for `host:port`.
    #[must_use]
    pub fn new(
        host: impl Into<String>,
        port: u16,
    ) -> Self {
        Self {
            host: host.into(),
            port,
        }
    }
}

impl NetworkPolicy {
    /// Check the Unix socket paths, the proxy, and every destination.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidPolicy`] for the first violation found.
    pub fn validate(&self) -> Result<(), InvalidPolicy> {
        for socket in &self.unix_sockets {
            path::check_absolute_normalized(socket)?;
        }
        if let Some(proxy) = &self.proxy {
            if proxy.port == 0 {
                return Err(InvalidPolicy::ZeroProxyPort);
            }
            if let Some(socket) = &proxy.socket {
                path::check_absolute_normalized(socket)?;
            }
            for destination in &proxy.egress {
                validate_destination(destination)?;
            }
        }
        Ok(())
    }
}

/// Check one egress destination.
fn validate_destination(destination: &HostPort) -> Result<(), InvalidPolicy> {
    if destination.host.is_empty() {
        return Err(InvalidPolicy::EmptyHost);
    }
    let host_is_invalid = destination
        .host
        .chars()
        .any(|ch| ch.is_whitespace() || ch == '\0');
    if host_is_invalid {
        return Err(InvalidPolicy::InvalidHost {
            host: destination.host.clone(),
        });
    }
    if destination.port == 0 {
        return Err(InvalidPolicy::ZeroPort {
            host: destination.host.clone(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    // Tests for the network domain: the zero value grants nothing, an opted-in
    // proxy with an empty destination set still reaches nothing, and every
    // destination is validated.

    use super::*;

    #[test]
    fn the_zero_policy_grants_no_socket_loopback_or_egress() {
        let network = NetworkPolicy::default();
        assert!(network.unix_sockets.is_empty());
        assert!(!network.loopback);
        assert!(network.proxy.is_none());
        assert!(network.validate().is_ok());
    }

    #[test]
    fn a_proxy_with_no_destinations_validates_but_reaches_nothing() {
        let network = NetworkPolicy {
            proxy: Some(ProxyGrant {
                port: 8080,
                socket: None,
                egress: Vec::new(),
            }),
            ..NetworkPolicy::default()
        };
        assert!(network.validate().is_ok());
    }

    #[test]
    fn a_zero_proxy_port_is_rejected() {
        let network = NetworkPolicy {
            proxy: Some(ProxyGrant {
                port: 0,
                socket: None,
                egress: Vec::new(),
            }),
            ..NetworkPolicy::default()
        };
        assert_eq!(network.validate(), Err(InvalidPolicy::ZeroProxyPort));
    }

    #[test]
    fn an_empty_host_is_rejected() {
        let network = NetworkPolicy {
            proxy: Some(ProxyGrant {
                port: 80,
                socket: None,
                egress: vec![HostPort::new("", 443)],
            }),
            ..NetworkPolicy::default()
        };
        assert_eq!(network.validate(), Err(InvalidPolicy::EmptyHost));
    }

    #[test]
    fn a_host_with_whitespace_is_rejected() {
        let network = NetworkPolicy {
            proxy: Some(ProxyGrant {
                port: 80,
                socket: None,
                egress: vec![HostPort::new("api example.com", 443)],
            }),
            ..NetworkPolicy::default()
        };
        assert!(matches!(
            network.validate(),
            Err(InvalidPolicy::InvalidHost { .. })
        ));
    }

    #[test]
    fn a_zero_port_is_rejected() {
        let network = NetworkPolicy {
            proxy: Some(ProxyGrant {
                port: 80,
                socket: None,
                egress: vec![HostPort::new("api.example.com", 0)],
            }),
            ..NetworkPolicy::default()
        };
        assert!(matches!(
            network.validate(),
            Err(InvalidPolicy::ZeroPort { .. })
        ));
    }

    #[test]
    fn a_relative_unix_socket_is_rejected() {
        let network = NetworkPolicy {
            unix_sockets: vec![PathBuf::from("relative.sock")],
            ..NetworkPolicy::default()
        };
        assert!(matches!(
            network.validate(),
            Err(InvalidPolicy::NotAbsolute { .. })
        ));
    }

    #[test]
    fn a_relative_proxy_socket_is_rejected() {
        let network = NetworkPolicy {
            proxy: Some(ProxyGrant {
                port: 8080,
                socket: Some(PathBuf::from("relative.sock")),
                egress: Vec::new(),
            }),
            ..NetworkPolicy::default()
        };
        assert!(matches!(
            network.validate(),
            Err(InvalidPolicy::NotAbsolute { .. })
        ));
    }
}
