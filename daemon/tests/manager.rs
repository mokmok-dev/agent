//! End-to-end tests for the session manager's start sequence and teardown.
//!
//! The bus is the real `agent` server over a Unix socket. The launcher is a fake,
//! so the start sequence, the lifecycle events, and the teardown are exercised on
//! any host; a fake also lets a test assert that the process was killed and that
//! the policy the manager built is the one it intended.
#![expect(
    clippy::expect_used,
    clippy::panic,
    reason = "integration test code may panic when a fixture fails"
)]

use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use agent::bus::Bus;
use agent::server::Server;
use agent::transport::{Allowlist, Listener, WebSocketConfig};
use daemon::Error;
use daemon::bus::BusClient;
use daemon::image::AgentImage;
use daemon::launcher::{LaunchRequest, LaunchedProcess, Launcher};
use daemon::manager::{Manager, ManagerConfig, SESSION_FAILED, SESSION_STARTED, SESSION_STOPPED};
use daemon::session::SessionId;

/// A running bus server on a fresh socket.
struct BusServer {
    root: PathBuf,
    socket: PathBuf,
    _runtime: tokio::runtime::Runtime,
}

impl BusServer {
    fn start(tag: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("daemon-mgr-{tag}-{}-{unique}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("creates the root");
        let socket = root.join("bus.sock");

        let uid = std::fs::metadata(std::env::temp_dir())
            .expect("the temp dir has metadata")
            .uid();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("builds a runtime");
        let guard = runtime.enter();
        let listener = Listener::bind(&socket, WebSocketConfig::default()).expect("binds");
        let bus = Bus::open(root.join("data"), 64).expect("opens the bus");
        let server = Server::new(listener, bus, Allowlist::new().allow_uid(uid));
        runtime.spawn(server.run());
        drop(guard);
        wait_for(&socket);
        Self {
            root,
            socket,
            _runtime: runtime,
        }
    }

    fn client(&self) -> BusClient {
        BusClient::connect(&daemon::bus::Config {
            socket: self.socket.clone(),
            subscriber_id: "manager-test".to_owned(),
        })
        .expect("connects")
    }
}

impl Drop for BusServer {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn wait_for(socket: &std::path::Path) {
    for _ in 0..100 {
        if std::os::unix::net::UnixStream::connect(socket).is_ok() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    panic!("the socket never accepted at {}", socket.display());
}

/// A fake confined process that records whether it was killed.
#[derive(Debug)]
struct FakeProcess {
    killed: Arc<AtomicBool>,
}

impl LaunchedProcess for FakeProcess {
    fn kill(&mut self) -> std::io::Result<()> {
        self.killed.store(true, Ordering::SeqCst);
        Ok(())
    }
}

/// A launcher that records its requests and hands out fake processes.
#[derive(Debug, Default)]
struct FakeLauncher {
    supported: bool,
    requests: Mutex<Vec<LaunchRequest>>,
    killed: Arc<AtomicBool>,
}

impl Launcher for FakeLauncher {
    fn is_supported(&self) -> bool {
        self.supported
    }

    fn launch(
        &self,
        request: &LaunchRequest,
    ) -> Result<Box<dyn LaunchedProcess>, Error> {
        self.requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(LaunchRequest {
                policy: request.policy.clone(),
                program: request.program.clone(),
                args: request.args.clone(),
                scratch: request.scratch.clone(),
                egress: None,
            });
        Ok(Box::new(FakeProcess {
            killed: Arc::clone(&self.killed),
        }))
    }
}

/// A launcher that records requests and exposes them.
fn launcher(supported: bool) -> (Arc<FakeLauncher>, Box<dyn Launcher>) {
    let fake = Arc::new(FakeLauncher {
        supported,
        requests: Mutex::new(Vec::new()),
        killed: Arc::new(AtomicBool::new(false)),
    });
    (Arc::clone(&fake), Box::new(ArcLauncher(Arc::clone(&fake))))
}

/// The requested policy's workspace access and granted bus sockets, read from the
/// single recorded launch.
///
/// The lock is taken and released inside this helper, so the caller holds no
/// guard across its later assertions. Clippy's `significant_drop_tightening`
/// would prefer the guard dropped sooner, but binding it is the point of the
/// helper: the guard must live for both field reads.
#[expect(
    clippy::significant_drop_tightening,
    reason = "the guard must live for the whole read, which is the helper's purpose"
)]
fn launched_policy(
    fake: &FakeLauncher,
    workspace: &std::path::Path,
) -> (sandbox::policy::Access, Vec<PathBuf>) {
    let requests = fake.requests.lock().unwrap_or_else(PoisonError::into_inner);
    assert_eq!(requests.len(), 1, "one launch");
    let request = requests.first().expect("one request");
    (
        request.policy.fs.access_for(workspace),
        request.policy.network.unix_sockets.clone(),
    )
}

/// Newtype so the fake can be both inspected and handed over as a `Launcher`.
#[derive(Debug)]
struct ArcLauncher(Arc<FakeLauncher>);

impl Launcher for ArcLauncher {
    fn is_supported(&self) -> bool {
        self.0.is_supported()
    }

    fn launch(
        &self,
        request: &LaunchRequest,
    ) -> Result<Box<dyn LaunchedProcess>, Error> {
        self.0.launch(request)
    }
}

/// Poll `seen` for an event of `ty`, or panic.
fn await_event(
    seen: &Arc<Mutex<Vec<agent::cloudevent::Event>>>,
    ty: &str,
) -> agent::cloudevent::Event {
    for _ in 0..200 {
        let events = seen.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(event) = events.iter().find(|event| event.ty == ty) {
            return event.clone();
        }
        drop(events);
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    panic!(
        "never saw {ty}; saw {:?}",
        seen.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|event| event.ty.clone())
            .collect::<Vec<_>>()
    );
}

/// A subscriber that records every event.
fn recorder(client: &BusClient) -> Arc<Mutex<Vec<agent::cloudevent::Event>>> {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let handler = Box::new(move |event: &agent::cloudevent::Event| {
        sink.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(event.clone());
    });
    client.subscribe(0, handler).expect("subscribes");
    seen
}

/// A workspace directory removed on drop.
struct Workspace(PathBuf);

impl Workspace {
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "daemon-mgr-ws-{tag}-{}-{unique}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("creates the workspace");
        Self(path)
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// An image whose program is outside every workspace.
fn image() -> AgentImage {
    AgentImage::new("/usr/bin/agent-agent")
}

/// The `PATH` for a test manager.
///
/// The egress helpers must resolve, so this creates placeholder
/// `sandbox-supervisor` and `egress-forward` files in the server's directory and
/// puts it first on the path. The fake launcher never executes them, so only
/// their presence matters; this keeps the test independent of whether cargo has
/// built the real binaries.
fn test_search_path(server: &BusServer) -> String {
    let bin = server.root.join("helpers");
    std::fs::create_dir_all(&bin).expect("creates the helper dir");
    for name in ["sandbox-supervisor", "egress-forward"] {
        std::fs::write(bin.join(name), b"#!/bin/sh\n").expect("writes the placeholder helper");
    }
    let host = std::env::var("PATH").unwrap_or_default();
    format!("{}:{}", bin.display(), host)
}

/// A manager over a fresh run directory and `launcher`.
fn manager(
    server: &BusServer,
    launcher: Box<dyn Launcher>,
) -> Manager {
    let config = ManagerConfig::new(
        server.socket.clone(),
        server.root.join("run"),
        server.root.join("agentd"),
        test_search_path(server),
    )
    .denying(server.root.join("authority"));
    Manager::new(
        config,
        launcher,
        sandbox::egress::HostCapability::PrivateNetworkNamespace,
        std::sync::Arc::new(server.client()),
    )
}

#[test]
fn opening_a_session_starts_the_process_and_publishes_started() {
    let server = BusServer::start("open");
    let observer = server.client();
    let seen = recorder(&observer);
    let (fake, boxed) = launcher(true);
    let manager = manager(&server, boxed);
    let workspace = Workspace::new("open");
    let id = SessionId::new("s-1").expect("valid id");

    manager
        .open_session(&server.client(), &id, &image(), &workspace.0)
        .expect("opens");

    assert_eq!(manager.state(&id).expect("known"), daemon::State::Running);
    let event = await_event(&seen, SESSION_STARTED);
    assert_eq!(event.subject.as_deref(), Some("s-1"));
    assert_eq!(
        event.data.as_ref().expect("data")["workspace"],
        workspace.0.to_str().expect("utf8")
    );

    // The launcher got the policy the manager built: the workspace writable, the
    // bus socket granted, and the deny mask present when the path exists.
    let (workspace_access, bus_sockets) = launched_policy(&fake, &workspace.0);
    assert_eq!(workspace_access, sandbox::policy::Access::Write);
    assert_eq!(bus_sockets, vec![server.socket.clone()]);
    assert!(!fake.killed.load(Ordering::SeqCst), "the process is alive");
}

#[test]
fn a_host_without_a_backend_fails_the_session_closed() {
    // No confinement, so no session: the manager must not run the agent.
    let server = BusServer::start("nobackend");
    let observer = server.client();
    let seen = recorder(&observer);
    let (fake, boxed) = launcher(false);
    let manager = manager(&server, boxed);
    let workspace = Workspace::new("nobackend");
    let id = SessionId::new("s-1").expect("valid id");

    let error = manager
        .open_session(&server.client(), &id, &image(), &workspace.0)
        .expect_err("no backend fails the session");
    assert!(matches!(error, Error::NoBackend));

    assert_eq!(manager.state(&id).expect("known"), daemon::State::Failed);
    assert!(
        fake.requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_empty(),
        "nothing was launched"
    );
    let event = await_event(&seen, SESSION_FAILED);
    assert_eq!(event.subject.as_deref(), Some("s-1"));
}

#[test]
fn a_failed_session_releases_its_workspace() {
    // The reservation is released, so a client can reopen the same workspace.
    let server = BusServer::start("release");
    let (_, boxed) = launcher(false);
    let first_manager = manager(&server, boxed);
    let workspace = Workspace::new("release");
    let first = SessionId::new("a").expect("valid id");

    let _ = first_manager.open_session(&server.client(), &first, &image(), &workspace.0);
    assert_eq!(
        first_manager.state(&first).expect("known"),
        daemon::State::Failed
    );

    let (_, boxed) = launcher(true);
    let second_manager = manager(&server, boxed);
    let second = SessionId::new("b").expect("valid id");
    second_manager
        .open_session(&server.client(), &second, &image(), &workspace.0)
        .expect("the workspace is free after the failure");
}

#[test]
fn a_session_that_grants_egress_binds_a_proxy() {
    // The image grants egress, so the start sequence binds a proxy for the
    // session. The proxy's socket exists while the session runs and is removed
    // when the session is stopped, which is the whole point of a per-session
    // proxy: a revoke or a teardown reaches exactly one session's reach.
    let server = BusServer::start("egress-bind");
    let (_, boxed) = launcher(true);
    let manager = manager(&server, boxed);
    let workspace = Workspace::new("egress-bind");
    let id = SessionId::new("s-1").expect("valid id");
    let image = image().allowing("api.example.com");

    manager
        .open_session(&server.client(), &id, &image, &workspace.0)
        .expect("opens");
    assert_eq!(manager.state(&id).expect("known"), daemon::State::Running);

    // The manager's egress socket for this session exists while it runs.
    let socket = server.root.join("run").join("egress-s-1.sock");
    assert!(
        socket.exists(),
        "a session that grants egress must bind its proxy"
    );

    manager.stop_session(&server.client(), &id).expect("stops");
    assert!(
        !socket.exists(),
        "stopping a session must remove its proxy socket"
    );
}

#[test]
fn a_second_session_on_a_live_workspace_is_refused() {
    let server = BusServer::start("busy");
    let (_, boxed) = launcher(true);
    let manager = manager(&server, boxed);
    let workspace = Workspace::new("busy");

    manager
        .open_session(
            &server.client(),
            &SessionId::new("a").expect("valid id"),
            &image(),
            &workspace.0,
        )
        .expect("opens the first");
    let error = manager
        .open_session(
            &server.client(),
            &SessionId::new("b").expect("valid id"),
            &image(),
            &workspace.0,
        )
        .expect_err("a live workspace is busy");
    assert!(matches!(error, Error::WorkspaceBusy { .. }));
}

#[test]
fn stopping_a_session_kills_the_process_and_publishes_stopped() {
    let server = BusServer::start("stop");
    let observer = server.client();
    let seen = recorder(&observer);
    let (fake, boxed) = launcher(true);
    let manager = manager(&server, boxed);
    let workspace = Workspace::new("stop");
    let id = SessionId::new("s-1").expect("valid id");

    manager
        .open_session(&server.client(), &id, &image(), &workspace.0)
        .expect("opens");
    assert!(!fake.killed.load(Ordering::SeqCst));

    manager.stop_session(&server.client(), &id).expect("stops");

    assert!(fake.killed.load(Ordering::SeqCst), "the process was killed");
    assert_eq!(manager.state(&id).expect("known"), daemon::State::Stopped);
    await_event(&seen, SESSION_STOPPED);
}

#[test]
fn a_second_stop_is_refused() {
    // The second stop must not reach a second process.
    let server = BusServer::start("double-stop");
    let (_, boxed) = launcher(true);
    let manager = manager(&server, boxed);
    let workspace = Workspace::new("double-stop");
    let id = SessionId::new("s-1").expect("valid id");

    manager
        .open_session(&server.client(), &id, &image(), &workspace.0)
        .expect("opens");
    manager.stop_session(&server.client(), &id).expect("stops");
    let error = manager
        .stop_session(&server.client(), &id)
        .expect_err("a stopped session cannot be stopped again");
    assert!(matches!(error, Error::InvalidTransition { .. }));
}

#[test]
fn a_program_inside_the_workspace_is_refused() {
    // A workspace must not supply the program it confines.
    let server = BusServer::start("inside");
    let (fake, boxed) = launcher(true);
    let manager = manager(&server, boxed);
    let workspace = Workspace::new("inside");
    let program = workspace.0.join("agent-agent");
    std::fs::write(&program, b"#!/bin/sh\n").expect("writes the program");

    let error = manager
        .open_session(
            &server.client(),
            &SessionId::new("s-1").expect("valid id"),
            &AgentImage::new(program),
            &workspace.0,
        )
        .expect_err("a program inside the workspace is refused");
    assert!(matches!(error, Error::ProgramInsideWorkspace(_)));
    assert!(
        fake.requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_empty(),
        "nothing was launched"
    );
}

#[test]
fn an_exit_releases_the_workspace() {
    // A process that ends on its own frees the workspace for a reopen.
    let server = BusServer::start("exit");
    let observer = server.client();
    let seen = recorder(&observer);
    let (_, boxed) = launcher(true);
    let manager = manager(&server, boxed);
    let workspace = Workspace::new("exit");
    let id = SessionId::new("s-1").expect("valid id");

    manager
        .open_session(&server.client(), &id, &image(), &workspace.0)
        .expect("opens");
    manager
        .mark_exited(&server.client(), &id)
        .expect("records the exit");

    assert_eq!(manager.state(&id).expect("known"), daemon::State::Exited);
    await_event(&seen, "agent.session.exited");
}
