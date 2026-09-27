//! Building the command's environment when egress is granted.
//!
//! A granted command discovers the proxy through the four conventional variables
//! an HTTP client already reads. The daemon **replaces** them, never appends: an
//! operator-authored `HTTP_PROXY` is dropped first, so the effective value cannot
//! depend on the order the environment happens to be built in. See
//! `docs/sandbox/network.md`.
//!
//! `NO_PROXY` keeps the command's own loopback off the tunnel. Without it an
//! agent routes its internal server calls through the proxy and its session setup
//! fails.

use crate::policy::EnvVar;

use super::{ProxyConfig, Transport, proxy_url};

/// The names the daemon owns when egress is granted.
pub const PROXY_ENV_NAMES: [&str; 3] = ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"];

/// The hosts exempted from the proxy: the command's own loopback.
pub const NO_PROXY: &str = "127.0.0.1,localhost,::1";

/// Inject the proxy variables into `env`, replacing any of the same name.
///
/// The value names the loopback address the command can actually reach: the
/// forwarder's port in the Unix-socket transport (the forwarder binds loopback
/// inside the namespace), or the proxy's own port in the loopback transport. The
/// URL carries the per-proxy token, so another process of the same user cannot
/// reuse the tunnel.
///
/// Injection is a replacement, not an append: every name in
/// [`PROXY_ENV_NAMES`] and `NO_PROXY` is removed from `env` before the daemon's
/// value is added, so a stale operator value can never win.
pub fn inject_proxy_env(
    config: &ProxyConfig,
    env: &mut Vec<EnvVar>,
) {
    let port = match &config.transport {
        // On Linux the command reaches the forwarder on loopback; the forwarder
        // carries the bytes to the socket.
        Transport::UnixSocket { forward_port, .. } => *forward_port,
        // A host without a namespace reaches the proxy's own loopback port.
        Transport::Loopback(port) => *port,
    };
    install(config, port, env);
}

/// Install the four variables for `port`, replacing any existing ones.
fn install(
    config: &ProxyConfig,
    port: u16,
    env: &mut Vec<EnvVar>,
) {
    // Drop every name the daemon owns, so the result is deterministic.
    env.retain(|var| !owns(&var.name));
    let url = proxy_url(port, &config.token);
    for name in PROXY_ENV_NAMES {
        env.push(EnvVar::new(name, &url));
    }
    env.push(EnvVar::new("NO_PROXY", NO_PROXY));
}

/// Whether the daemon owns `name`.
///
/// The comparison is case-insensitive: an HTTP client may read the lower-case
/// spelling, and the environment is not consistently cased across tools.
fn owns(name: &str) -> bool {
    name.eq_ignore_ascii_case("NO_PROXY")
        || PROXY_ENV_NAMES
            .iter()
            .any(|owned| name.eq_ignore_ascii_case(owned))
}

#[cfg(test)]
mod tests {
    // Tests for the replacement semantics: a stale value cannot win, the URL
    // names the reachable port, and only the daemon's names are touched.

    use super::*;
    use crate::egress::bearer;
    use std::path::PathBuf;

    /// A config with the token `tok`, a forward port, and a socket.
    fn config() -> ProxyConfig {
        ProxyConfig {
            transport: Transport::UnixSocket {
                socket: PathBuf::from("/run/egress.sock"),
                forward_port: 8080,
            },
            token: "tok".to_owned(),
            rules: std::sync::Arc::new(crate::egress::Allowlist::empty()),
            approver: None,
        }
    }

    /// The value of `name` in `env`, if present.
    fn value<'a>(
        env: &'a [EnvVar],
        name: &str,
    ) -> Option<&'a str> {
        env.iter()
            .find(|var| var.name.eq_ignore_ascii_case(name))
            .map(|var| var.value.as_str())
    }

    /// How many entries `env` has named `name`.
    fn count(
        env: &[EnvVar],
        name: &str,
    ) -> usize {
        env.iter()
            .filter(|var| var.name.eq_ignore_ascii_case(name))
            .count()
    }

    #[test]
    fn the_unix_socket_transport_names_the_forward_port() {
        let mut env = Vec::new();
        inject_proxy_env(&config(), &mut env);
        assert_eq!(
            value(&env, "HTTP_PROXY"),
            Some("http://agent:tok@127.0.0.1:8080")
        );
    }

    #[test]
    fn the_loopback_transport_names_the_proxy_port() {
        let mut config = config();
        config.transport = Transport::Loopback(9999);
        let mut env = Vec::new();
        inject_proxy_env(&config, &mut env);
        assert_eq!(
            value(&env, "HTTP_PROXY"),
            Some("http://agent:tok@127.0.0.1:9999")
        );
    }

    #[test]
    fn all_three_proxy_names_and_no_proxy_are_set() {
        let mut env = Vec::new();
        inject_proxy_env(&config(), &mut env);
        assert_eq!(count(&env, "HTTP_PROXY"), 1);
        assert_eq!(count(&env, "HTTPS_PROXY"), 1);
        assert_eq!(count(&env, "ALL_PROXY"), 1);
        assert_eq!(value(&env, "NO_PROXY"), Some(NO_PROXY));
    }

    #[test]
    fn an_operator_value_is_replaced_not_appended() {
        let mut env = vec![
            EnvVar::new("HTTP_PROXY", "http://evil.example:1"),
            EnvVar::new("HTTPS_PROXY", "http://evil.example:1"),
        ];
        inject_proxy_env(&config(), &mut env);
        assert_eq!(count(&env, "HTTP_PROXY"), 1);
        assert_eq!(count(&env, "HTTPS_PROXY"), 1);
        assert_eq!(
            value(&env, "HTTP_PROXY"),
            Some("http://agent:tok@127.0.0.1:8080")
        );
        assert!(!env.iter().any(|var| var.value.contains("evil.example")));
    }

    #[test]
    fn the_replacement_is_case_insensitive() {
        let mut env = vec![EnvVar::new("http_proxy", "http://evil.example:1")];
        inject_proxy_env(&config(), &mut env);
        assert!(
            !env.iter().any(|var| var.value.contains("evil.example")),
            "a lower-case operator value must be dropped too"
        );
    }

    #[test]
    fn an_unrelated_variable_is_left_alone() {
        let mut env = vec![EnvVar::new("PATH", "/usr/bin")];
        inject_proxy_env(&config(), &mut env);
        assert_eq!(value(&env, "PATH"), Some("/usr/bin"));
    }

    #[test]
    fn no_proxy_keeps_the_command_loopback_off_the_tunnel() {
        let mut env = Vec::new();
        inject_proxy_env(&config(), &mut env);
        let value = value(&env, "NO_PROXY").expect("NO_PROXY is set");
        assert!(value.contains("127.0.0.1"));
        assert!(value.contains("localhost"));
        assert!(value.contains("::1"));
    }

    #[test]
    fn the_url_carries_the_bearer_token() {
        // The `UserInfo` the proxy expects as `Proxy-Authorization`.
        let mut env = Vec::new();
        inject_proxy_env(&config(), &mut env);
        let url = value(&env, "HTTP_PROXY").expect("HTTP_PROXY is set");
        assert!(url.contains(&format!("agent:{}@", config().token)));
        // `bearer` is the form the proxy compares against.
        assert_eq!(bearer("tok"), "Bearer tok");
    }
}
