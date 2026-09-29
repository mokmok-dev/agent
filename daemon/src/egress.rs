//! The per-session egress proxy: binding it and serving it while the session runs.
//!
//! A session whose image allows a host needs a proxy of its own. The manager
//! binds the proxy **before** the policy renders, because the renderer requires
//! the socket to exist, then serves it on a thread until the session ends. The
//! proxy is the one runtime-mutable part of a session: an authority adds and
//! revokes destinations on the shared [`Allowlist`], and a revoke closes the
//! tunnels its rule granted. See `docs/session/daemon.md`.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use sandbox::egress::{
    Allowlist, Approver, Desk, DestinationSet, HostCapability, ProxyConfig, UnixProxy,
    select_transport,
};
use sandbox::policy::ProxyGrant;

use crate::Error;
use crate::bus::{BusClient, BusPublisher};
use crate::image::AgentImage;

/// The loopback port the confined command's `HTTP_PROXY` names.
///
/// The forwarder binds this inside the session's private network namespace, so
/// every session uses the same number; they do not collide, because each session
/// has its own namespace and its own loopback.
pub const FORWARD_PORT: u16 = 8080;

/// A session's running egress proxy.
#[derive(Debug)]
pub struct Egress {
    /// The socket the proxy listens on, mounted into the session's namespace.
    pub socket: PathBuf,
    /// The loopback port the command's `HTTP_PROXY` names.
    pub port: u16,
    /// The per-proxy token a client must present.
    token: String,
    /// The mutable destination set, shared with the authority that edits it.
    pub rules: Arc<Allowlist>,
    /// Set to stop the serve loop.
    stop: Arc<AtomicBool>,
    /// The serve thread, joined on drop.
    serve: Option<std::thread::JoinHandle<()>>,
}

impl Egress {
    /// Bind a proxy for `image` and start serving it.
    ///
    /// The proxy's token is opaque and per proxy; the approval requests it
    /// publishes go over `bus`, and an unlisted destination is asked about rather
    /// than refused, so an authority can widen a session while it runs.
    ///
    /// # Errors
    ///
    /// Returns [`Error::EgressRefused`] when the host cannot confine egress to a
    /// single route, and [`Error::Egress`] when the socket cannot be bound.
    pub fn start(
        image: &AgentImage,
        socket: PathBuf,
        capability: &HostCapability,
        bus: &Arc<BusClient>,
        sandbox_id: &str,
        deadline: Duration,
    ) -> Result<Self, Error> {
        Self::start_with_rules(
            DestinationSet::of(image.egress_rules()),
            socket,
            capability,
            bus,
            sandbox_id,
            deadline,
        )
    }

    /// Bind a proxy with an explicit starting destination set.
    ///
    /// Lets a caller name a port the image's `allowing` helper does not, which a
    /// test needs because it cannot bind port 443.
    ///
    /// # Errors
    ///
    /// As [`Egress::start`].
    pub fn start_with_rules(
        rules: DestinationSet,
        socket: PathBuf,
        capability: &HostCapability,
        bus: &Arc<BusClient>,
        sandbox_id: &str,
        deadline: Duration,
    ) -> Result<Self, Error> {
        let rules = Arc::new(Allowlist::new(rules));
        let approver: Arc<dyn Approver> = Arc::new(Desk::new(
            Arc::new(BusPublisher::new(Arc::clone(bus))),
            sandbox_id.to_owned(),
            deadline,
        ));
        let token = crate::bus::mint_token();
        // The forwarder binds inside the session's private network namespace, so
        // a fixed port is correct: each session has its own namespace and its own
        // loopback, and the port is only reachable there. A port of zero would
        // also make the policy invalid, because `ProxyGrant.port` must be known
        // when the renderer injects `HTTP_PROXY`.
        let transport =
            select_transport(capability, socket, FORWARD_PORT).map_err(|_| Error::EgressRefused)?;
        let port = match &transport {
            sandbox::egress::Transport::UnixSocket { forward_port, .. } => *forward_port,
            sandbox::egress::Transport::Loopback(port) => *port,
        };
        let proxy = UnixProxy::bind(ProxyConfig {
            transport,
            token: token.clone(),
            rules: Arc::clone(&rules),
            approver: Some(approver),
        })
        .map_err(|error| Error::Egress(error.to_string()))?;
        let socket = proxy.path().to_path_buf();

        let stop = Arc::new(AtomicBool::new(false));
        let serve = serve(proxy, Arc::clone(&stop))?;
        Ok(Self {
            socket,
            port,
            token,
            rules,
            stop,
            serve: Some(serve),
        })
    }

    /// The per-proxy token a client must present as `Proxy-Authorization`.
    #[must_use]
    pub fn token(&self) -> &str {
        &self.token
    }

    /// The egress grant the policy needs, naming the proxy's socket and port.
    #[must_use]
    pub fn grant(
        &self,
        image: &AgentImage,
    ) -> ProxyGrant {
        ProxyGrant {
            port: self.port,
            socket: Some(self.socket.clone()),
            egress: image.egress_rules(),
        }
    }
}

impl Drop for Egress {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the accept loop so it observes `stop` and returns.
        let _ = std::os::unix::net::UnixStream::connect(&self.socket);
        if let Some(serve) = self.serve.take() {
            let _ = serve.join();
        }
    }
}

/// Serve the proxy on its own thread until `stop` is set.
///
/// The accept loop **dispatches** each accepted connection to its own thread, so
/// a long-lived tunnel never blocks the next accept. That is what lets `stop`
/// end the loop promptly and lets `Egress::drop` join it: the loop only ever
/// waits on `accept`, never inside a tunnel. A tunnel thread ends when its
/// client closes, which the session's teardown guarantees by killing the
/// process first.
///
/// The listener is non-blocking, and the loop sleeps briefly on an empty accept,
/// so it observes `stop` without depending on a wake-up connection.
///
/// # Errors
///
/// Returns [`Error::Egress`] if the listener cannot be cloned or made
/// non-blocking, or the serve thread cannot be spawned.
fn serve(
    proxy: UnixProxy,
    stop: Arc<AtomicBool>,
) -> Result<std::thread::JoinHandle<()>, Error> {
    let listener = proxy
        .listener()
        .try_clone()
        .map_err(|error| Error::Egress(error.to_string()))?;
    listener
        .set_nonblocking(true)
        .map_err(|error| Error::Egress(error.to_string()))?;
    let config = proxy.config().clone();
    std::thread::Builder::new()
        .name("daemon-egress".to_owned())
        .spawn(move || {
            // The proxy is held for the loop's lifetime, so its socket is
            // removed when the loop ends.
            let _proxy = proxy;
            while !stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let config = config.clone();
                        let _ = std::thread::Builder::new()
                            .name("daemon-egress-conn".to_owned())
                            .spawn(move || {
                                let _ = sandbox::egress::serve(&stream, &config);
                            });
                    },
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) =>
                    {
                        std::thread::sleep(Duration::from_millis(10));
                    },
                    Err(_) => std::thread::sleep(Duration::from_millis(10)),
                }
            }
        })
        .map_err(|error| Error::Egress(error.to_string()))
}
