//! End-to-end test of the session manager against a real confined process.
//!
//! Unlike `manager.rs`, this uses the real `SandboxLauncher`, so it needs a host
//! that can build a namespace. Each test detects the backend first and returns
//! early when it cannot, which is the same condition under which the manager
//! would refuse to open a session. On such a host the assertions would be
//! meaningless, so skipping is the honest outcome rather than a failure.
#![expect(
    clippy::expect_used,
    clippy::panic,
    reason = "integration test code may panic when a fixture fails"
)]

use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use agent::bus::Bus;
use agent::server::Server;
use agent::transport::{Allowlist, Listener, WebSocketConfig};
use daemon::bus::BusClient;
use daemon::image::AgentImage;
use daemon::launcher::{Launcher, SandboxLauncher};
use daemon::manager::{Manager, ManagerConfig};
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
            std::env::temp_dir().join(format!("daemon-real-{tag}-{}-{unique}", std::process::id()));
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
            subscriber_id: "real-test".to_owned(),
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
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("the socket never accepted at {}", socket.display());
}

/// A workspace removed on drop, with a marker path inside it.
struct Workspace(PathBuf);

impl Workspace {
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "daemon-real-ws-{tag}-{}-{unique}",
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

/// A real launcher, or `None` when this host cannot confine a command.
fn launcher_or_skip(search_path: &str) -> Option<Box<dyn Launcher>> {
    let launcher = SandboxLauncher::detect(search_path, &[]);
    launcher
        .is_supported()
        .then(|| Box::new(launcher) as Box<dyn Launcher>)
}

/// A manager whose launcher is real and whose image runs `/bin/sh`.
fn manager(
    server: &BusServer,
    search_path: &str,
) -> Option<Manager> {
    let launcher = launcher_or_skip(search_path)?;
    let capability = sandbox::egress::HostCapability::of(&sandbox::filesystem::Backend::detect(
        search_path,
        &[],
    ));
    let config = ManagerConfig::new(
        server.socket.clone(),
        server.root.join("run"),
        server.root.join("agentd"),
        search_path.to_owned(),
    );
    Some(Manager::new(
        config,
        launcher,
        capability,
        std::sync::Arc::new(server.client()),
    ))
}

fn host_path() -> String {
    std::env::var("PATH").unwrap_or_else(|_| "/bin:/usr/bin".to_owned())
}

#[test]
fn a_script_writes_inside_its_workspace_and_is_denied_outside_it() {
    // The user-facing claim: a shell script runs in the sandbox, can write its
    // workspace, and cannot write outside it.
    let search_path = host_path();
    let server = BusServer::start("script");
    let Some(manager) = manager(&server, &search_path) else {
        return;
    };
    let workspace = Workspace::new("script");
    let outside = server.root.join("outside.txt");
    let marker = workspace.0.join("inside.txt");

    // The script writes inside, then tries outside; the second write fails and
    // the first succeeds. The exit code reports the second write's failure.
    let script = format!(
        "echo inside > {inside} || exit 2\necho outside > {outside} 2>/dev/null && exit 3\nexit 0",
        inside = marker.display(),
        outside = outside.display(),
    );
    // `/bin/sh` is the program; the args carry the script.
    let image = AgentImage::new("/bin/sh")
        .with_arg("-c")
        .with_arg(script)
        .with_env("PATH", &search_path);

    let id = SessionId::new("s-1").expect("valid id");
    manager
        .open_session(&server.client(), &id, &image, &workspace.0)
        .expect("opens a confined session");
    assert_eq!(manager.state(&id).expect("known"), daemon::State::Running);

    // The confined process is a shell that exits quickly; wait for the marker it
    // is allowed to write, then assert the outside write never happened. The
    // budget is generous because a `bwrap` spawn under a loaded host is slow, and
    // the assertion is about the state finally reached, not how fast.
    let mut wrote_inside = false;
    for _ in 0..800 {
        if marker.exists() {
            wrote_inside = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(wrote_inside, "the script wrote inside its workspace");
    assert_eq!(
        std::fs::read_to_string(&marker).expect("the marker is readable"),
        "inside\n"
    );
    assert!(
        !outside.exists(),
        "the script must not write outside its workspace"
    );

    // A stop on a process that already exited is refused, because the process is
    // gone; the important part is that opening and observing worked.
    let _ = manager.stop_session(&server.client(), &id);
}

#[test]
fn stopping_a_real_session_kills_its_process() {
    // The kill path over a real confined process: a background writer that would
    // leave a marker after a delay must not, because stopping kills the tree.
    let search_path = host_path();
    let server = BusServer::start("real-stop");
    let Some(manager) = manager(&server, &search_path) else {
        return;
    };
    let workspace = Workspace::new("real-stop");
    let late = workspace.0.join("late.txt");
    let ready = workspace.0.join("ready.txt");

    // The script starts a background writer, then sleeps; killing the process
    // must take the background writer with it. It writes the ready marker first,
    // so the stop happens against a process that is up rather than racing the
    // spawn under a loaded host.
    let script = format!(
        "echo up > {ready}\n(sleep 2; echo late > {late}) & sleep 30",
        ready = ready.display(),
        late = late.display()
    );
    let image = AgentImage::new("/bin/sh")
        .with_arg("-c")
        .with_arg(script)
        .with_env("PATH", &search_path);

    let id = SessionId::new("s-1").expect("valid id");
    manager
        .open_session(&server.client(), &id, &image, &workspace.0)
        .expect("opens");
    assert_eq!(manager.state(&id).expect("known"), daemon::State::Running);

    // Wait until the script is demonstrably running.
    let mut up = false;
    for _ in 0..800 {
        if ready.exists() {
            up = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(up, "the script reached its ready marker before the stop");

    manager.stop_session(&server.client(), &id).expect("stops");
    assert_eq!(manager.state(&id).expect("known"), daemon::State::Stopped);

    // Past the background writer's delay: if the kill did not reach the
    // descendant, the marker appears and this fails.
    std::thread::sleep(Duration::from_millis(2500));
    assert!(
        !late.exists(),
        "a stopped session must not leave a descendant's marker"
    );
}
