//! End-to-end test for the agent program over the real bus.
//!
//! The agent binary runs as its own process, addressed to a session by subject,
//! with the bus server the daemon would run. A test client publishes a command and
//! reads the agent's output, which is the whole interface the design gives the
//! agent.
#![expect(
    clippy::expect_used,
    clippy::panic,
    reason = "integration test code may panic when a fixture fails"
)]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent::bus::Bus;
use agent::client::{Client, Config as BusConfig, Handler};
use agent::cloudevent::{Event as CloudEvent, Incoming, SpecVersion};
use agent::server::Server;
use agent::transport::{Allowlist, Listener, WebSocketConfig};
use serde_json::json;

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
            std::env::temp_dir().join(format!("agent-e2e-{tag}-{}-{unique}", std::process::id()));
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

    fn client(&self) -> Client {
        Client::connect(&BusConfig {
            socket: self.socket.clone(),
            subscriber_id: "e2e-test".to_owned(),
        })
        .expect("connects")
    }
}

impl Drop for BusServer {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Wait until the socket accepts a connection.
fn wait_for(socket: &Path) {
    for _ in 0..100 {
        if std::os::unix::net::UnixStream::connect(socket).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("the socket never accepted at {}", socket.display());
}

/// An incoming event of `ty`.
fn incoming(
    ty: &str,
    subject: &str,
    data: serde_json::Value,
) -> Incoming {
    Incoming {
        specversion: SpecVersion::V1_0,
        ty: ty.to_owned(),
        source: None,
        id: None,
        time: None,
        subject: Some(subject.to_owned()),
        datacontenttype: None,
        sequence: None,
        data: Some(data),
        extensions: std::collections::BTreeMap::new(),
    }
}

/// A handler that records every event.
fn recorder() -> (Handler, Arc<Mutex<Vec<CloudEvent>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let handler = Box::new(move |event: &CloudEvent| {
        sink.lock().expect("unpoisoned").push(event.clone());
    });
    (handler, seen)
}

/// Poll `seen` for an event matching `predicate`, or fail.
fn await_event<F>(
    seen: &Arc<Mutex<Vec<CloudEvent>>>,
    what: &str,
    predicate: F,
) -> CloudEvent
where
    F: Fn(&CloudEvent) -> bool,
{
    for _ in 0..300 {
        let events = seen.lock().expect("unpoisoned").clone();
        if let Some(event) = events.iter().find(|event| predicate(event)) {
            return event.clone();
        }
        drop(events);
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("never saw {what}");
}

/// Whether `event` is an output event for `session` whose `kind` is `kind`.
///
/// The agent publishes progress while a command runs and its result when it
/// ends, so a test that wants the result has to say so.
fn is_output(
    event: &CloudEvent,
    session: &str,
    kind: &str,
) -> bool {
    event.ty == "agent.session.output"
        && event.subject.as_deref() == Some(session)
        && event
            .data
            .as_ref()
            .is_some_and(|data| data["kind"] == json!(kind))
}

/// A running agent binary, killed on drop.
struct Agent {
    child: Child,
}

impl Agent {
    /// Start the agent for `session` in `workspace`, replaying from `from_seq`.
    fn start(
        server: &BusServer,
        session: &str,
        workspace: &Path,
        from_seq: u64,
    ) -> Self {
        Self::start_with(server, session, workspace, from_seq, &[], &[])
    }

    /// Start the agent with extra arguments and environment.
    ///
    /// A session's model endpoint arrives this way: the daemon passes the flags and
    /// the proxy, and the key is already in the child's environment.
    fn start_with(
        server: &BusServer,
        session: &str,
        workspace: &Path,
        from_seq: u64,
        args: &[String],
        env: &[(&str, String)],
    ) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agent-agent"));
        command
            .arg("--socket")
            .arg(&server.socket)
            .arg("--session")
            .arg(session)
            .arg("--workdir")
            .arg(workspace)
            .arg("--from-seq")
            .arg(from_seq.to_string());
        for arg in args {
            command.arg(arg);
        }
        // The test run's own proxy variables must not decide what the child does.
        for name in PROXY_ENV {
            command.env_remove(name);
        }
        command.env("RUST_LOG", "info");
        for (name, value) in env {
            command.env(name, value);
        }
        let child = command.spawn().expect("the agent binary starts");
        Self { child }
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A temp workspace, removed on drop.
struct Workspace(PathBuf);

impl Workspace {
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("agent-ws-{tag}-{}-{unique}", std::process::id()));
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

#[test]
fn an_agent_runs_a_shell_command_and_reports_its_output() {
    let server = BusServer::start("shell");
    let observer = server.client();
    let (handler, seen) = recorder();
    observer.subscribe(0, handler).expect("subscribes");
    let workspace = Workspace::new("shell");

    // The agent replays the whole log, so the command must be in it first.
    let client = server.client();
    client
        .publish(incoming(
            "agent.session.command",
            "s-1",
            json!({"action": "shell", "detail": {"argv": ["/bin/sh", "-c", "echo hi"]}}),
        ))
        .expect("publishes the command");

    let _agent = Agent::start(&server, "s-1", &workspace.0, 0);

    let output = await_event(&seen, "an output event", |event| {
        is_output(event, "s-1", "done")
    });
    assert_eq!(
        output.data.as_ref().expect("data")["kind"],
        json!("done"),
        "the shell capability reports done"
    );
    assert_eq!(output.data.as_ref().expect("data")["action"], "shell");
    assert_eq!(
        output.data.as_ref().expect("data")["detail"]["stdout"],
        "hi\n",
        "the command's output is reported"
    );
    assert_eq!(output.data.as_ref().expect("data")["detail"]["code"], 0);
}

#[test]
fn a_shell_command_runs_in_the_session_workspace() {
    // The workdir is the session's workspace, so a relative write lands there and
    // the test can see it on the host.
    let server = BusServer::start("workdir");
    let observer = server.client();
    let (handler, seen) = recorder();
    observer.subscribe(0, handler).expect("subscribes");
    let workspace = Workspace::new("workdir");

    let client = server.client();
    client
        .publish(incoming(
            "agent.session.command",
            "s-w",
            json!({"action": "shell", "detail": {"argv": ["/bin/sh", "-c", "echo x > made.txt"]}}),
        ))
        .expect("publishes the command");

    let _agent = Agent::start(&server, "s-w", &workspace.0, 0);
    await_event(&seen, "the command's output", |event| {
        is_output(event, "s-w", "done")
    });

    assert_eq!(
        std::fs::read_to_string(workspace.0.join("made.txt")).expect("the file is on the host"),
        "x\n",
        "the command ran in the session's workspace"
    );
}

#[test]
fn an_agent_survives_a_restart_without_losing_a_command() {
    // A command published while no agent is running is durable. An agent that
    // starts later replays it from its cursor, so nothing is lost: this is the
    // property the design gives a bus peer.
    let server = BusServer::start("restart");
    let observer = server.client();
    let (handler, seen) = recorder();
    observer.subscribe(0, handler).expect("subscribes");
    let workspace = Workspace::new("restart");

    // Publish the command with no agent running.
    let client = server.client();
    client
        .publish(incoming(
            "agent.session.command",
            "s-2",
            json!({"action": "shell", "detail": {"argv": ["/bin/sh", "-c", "echo replayed"]}}),
        ))
        .expect("publishes the command");

    // Start the agent after the command exists; it must still receive it.
    let _agent = Agent::start(&server, "s-2", &workspace.0, 0);

    let output = await_event(&seen, "the replayed command's output", |event| {
        is_output(event, "s-2", "done")
    });
    assert_eq!(output.data.as_ref().expect("data")["action"], "shell");
    assert_eq!(
        output.data.as_ref().expect("data")["detail"]["stdout"],
        "replayed\n",
        "the command published before the agent started was replayed and run"
    );
}

#[test]
fn an_agent_ignores_a_command_addressed_to_another_session() {
    // Sessions share one bus; the subject is what scopes a command. An agent for
    // `s-3` must not act on a command for `s-4`.
    let server = BusServer::start("scoped");
    let observer = server.client();
    let (handler, seen) = recorder();
    observer.subscribe(0, handler).expect("subscribes");
    let workspace = Workspace::new("scoped");

    let client = server.client();
    client
        .publish(incoming(
            "agent.session.command",
            "s-4",
            json!({"action": "shell", "detail": {"argv": ["/bin/sh", "-c", "echo leaked"]}}),
        ))
        .expect("publishes a command for another session");

    let _agent = Agent::start(&server, "s-3", &workspace.0, 0);
    std::thread::sleep(Duration::from_millis(400));

    assert!(
        !seen
            .lock()
            .expect("unpoisoned")
            .iter()
            .any(|event| event.ty == "agent.session.output"
                && event.subject.as_deref() == Some("s-3")),
        "an agent must not act on another session's command"
    );
    assert!(
        !workspace.0.join("leaked").exists(),
        "the other session's command must not run here"
    );
}

#[test]
fn an_agent_reports_an_unknown_action_without_running_anything() {
    // A client and an agent of different versions must not crash each other: an
    // action the agent has no capability for is reported, not ignored.
    let server = BusServer::start("unknown");
    let observer = server.client();
    let (handler, seen) = recorder();
    observer.subscribe(0, handler).expect("subscribes");
    let workspace = Workspace::new("unknown");

    let client = server.client();
    client
        .publish(incoming(
            "agent.session.command",
            "s-u",
            json!({"action": "teleport", "detail": {}}),
        ))
        .expect("publishes the command");

    let _agent = Agent::start(&server, "s-u", &workspace.0, 0);

    let output = await_event(&seen, "the unknown action's output", |event| {
        is_output(event, "s-u", "error")
    });
    assert_eq!(output.data.as_ref().expect("data")["kind"], "error");
    assert_eq!(
        output.data.as_ref().expect("data")["detail"]["reason"],
        "the agent has no capability for this action"
    );
}

#[test]
fn an_agent_reports_a_command_before_it_runs() {
    // A command may run for minutes, so the agent announces it while it runs
    // rather than reporting only the result when it ends.
    let server = BusServer::start("progress");
    let observer = server.client();
    let (handler, seen) = recorder();
    observer.subscribe(0, handler).expect("subscribes");
    let workspace = Workspace::new("progress");

    let client = server.client();
    client
        .publish(incoming(
            "agent.session.command",
            "s-p",
            json!({"action": "shell", "detail": {"argv": ["/bin/sh", "-c", "echo hi"]}}),
        ))
        .expect("publishes the command");

    let _agent = Agent::start(&server, "s-p", &workspace.0, 0);

    let progress = await_event(&seen, "a progress event", |event| {
        is_output(event, "s-p", "progress")
    });
    assert_eq!(progress.data.as_ref().expect("data")["action"], "shell");
    assert_eq!(
        progress.data.as_ref().expect("data")["detail"]["argv"],
        json!(["/bin/sh", "-c", "echo hi"]),
        "the report names the argv that is about to run"
    );

    // The report arrives before the result, which is what makes the work visible
    // while it runs rather than only when it ends.
    await_event(&seen, "the command's result", |event| {
        is_output(event, "s-p", "done")
    });
    let events = seen.lock().expect("unpoisoned").clone();
    let at = |kind: &str| {
        events
            .iter()
            .position(|event| is_output(event, "s-p", kind))
            .expect("the event was awaited")
    };
    assert!(
        at("progress") < at("done"),
        "the report comes before the result"
    );
}

/// The variables a client reads to find its proxy.
const PROXY_ENV: [&str; 4] = ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "NO_PROXY"];

/// A proxy that records the `CONNECT` it is asked for, answers it, and then sends
/// nonsense.
///
/// It stands in for the session's egress proxy without any TLS at all: what the
/// agent does *before* the tunnel matters here — the address it asks for and the
/// token it authenticates with — and the nonsense makes the handshake fail, which
/// is the failure the task reports.
struct RecordingProxy {
    /// The port it listens on.
    port: u16,
    /// The head of the `CONNECT` it was asked for.
    head: Arc<Mutex<Option<String>>>,
}

impl RecordingProxy {
    /// Serve one tunnel.
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("binds");
        let port = listener.local_addr().expect("has an address").port();
        let head = Arc::new(Mutex::new(None));
        let sink = Arc::clone(&head);
        std::thread::spawn(move || {
            let Ok((mut client, _)) = listener.accept() else {
                return;
            };
            if let Ok(read) = read_head(&mut client) {
                *sink.lock().expect("unpoisoned") = Some(read);
            }
            let _ = client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n");
            let _ = client.write_all(b"not the endpoint");
            let _ = client.flush();
        });
        Self { port, head }
    }

    /// The URL the sandbox would inject for this proxy, carrying `token`.
    fn url(
        &self,
        token: &str,
    ) -> String {
        format!("http://agent:{token}@127.0.0.1:{}", self.port)
    }

    /// The `CONNECT` head this proxy was asked for.
    fn head(&self) -> String {
        self.head
            .lock()
            .expect("unpoisoned")
            .clone()
            .expect("the proxy was asked for a tunnel")
    }
}

/// Read a head, up to and including its blank line.
fn read_head(stream: &mut impl Read) -> std::io::Result<String> {
    let mut head = Vec::new();
    let mut byte = [0_u8; 1];
    loop {
        if stream.read(&mut byte)? == 0 {
            return Err(std::io::Error::other("the connection ended early"));
        }
        head.push(byte[0]);
        if head.ends_with(b"\r\n\r\n") {
            return Ok(String::from_utf8_lossy(&head).into_owned());
        }
        if head.len() > 16 * 1024 {
            return Err(std::io::Error::other("the head is too large"));
        }
    }
}

/// Run the agent with `args`, and return its status and its stderr, bounded.
fn run_agent(args: &[&str]) -> (std::process::ExitStatus, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_agent-agent"))
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the agent binary starts");
    // Bounded while reading: a child that writes without end must not grow this
    // test's memory. See decision-trail row 14.
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .expect("stderr is piped")
        .take(4096)
        .read_to_string(&mut stderr)
        .expect("reads stderr");
    let status = child.wait().expect("waits for the child");
    (status, stderr)
}

#[test]
fn an_agent_without_an_endpoint_reports_task_as_unknown() {
    // The client landing leaves the binary's behaviour unchanged: without the flags
    // there is no model, and `task` is an unknown action like any other.
    let server = BusServer::start("no-endpoint");
    let observer = server.client();
    let (handler, seen) = recorder();
    observer.subscribe(0, handler).expect("subscribes");
    let workspace = Workspace::new("no-endpoint");

    let client = server.client();
    client
        .publish(incoming(
            "agent.session.command",
            "s-t",
            json!({"action": "task", "detail": {"task": "count the files"}}),
        ))
        .expect("publishes the command");

    let _agent = Agent::start(&server, "s-t", &workspace.0, 0);
    let output = await_event(&seen, "the unknown action's output", |event| {
        is_output(event, "s-t", "error")
    });
    assert_eq!(output.data.as_ref().expect("data")["action"], "task");
    assert_eq!(
        output.data.as_ref().expect("data")["detail"]["reason"],
        "the agent has no capability for this action",
        "without an endpoint, `task` answers like any unknown action"
    );
}

#[test]
fn an_agent_with_an_endpoint_asks_the_proxy_for_it_with_the_bearer_header() {
    // The wiring: the three flags build the client, the proxy comes from the
    // environment, and the `CONNECT` carries the form the sandbox's proxy
    // authenticates. The tunnel then answers nonsense, so the handshake fails —
    // which the task reports, naming the endpoint that could not be reached.
    let server = BusServer::start("endpoint");
    let observer = server.client();
    let (handler, seen) = recorder();
    observer.subscribe(0, handler).expect("subscribes");
    let workspace = Workspace::new("endpoint");

    let client = server.client();
    client
        .publish(incoming(
            "agent.session.command",
            "s-e",
            json!({"action": "task", "detail": {"task": "say hello"}}),
        ))
        .expect("publishes the command");

    let proxy = RecordingProxy::start();
    let _agent = Agent::start_with(
        &server,
        "s-e",
        &workspace.0,
        0,
        &[
            "--model".to_owned(),
            "grok-4.7".to_owned(),
            "--base-url".to_owned(),
            "https://api.x.ai/v1".to_owned(),
            "--env-key".to_owned(),
            "PROVIDER_KEY".to_owned(),
        ],
        &[
            ("PROVIDER_KEY", "the-key".to_owned()),
            ("HTTPS_PROXY", proxy.url("proxy-token")),
        ],
    );

    let output = await_event(&seen, "the failed task's output", |event| {
        is_output(event, "s-e", "error")
    });
    let detail = &output.data.as_ref().expect("data")["detail"];
    assert_eq!(
        detail["reason"], "the model call failed",
        "the task reports what the model call did: {detail}"
    );
    assert!(
        detail["error"]
            .as_str()
            .is_some_and(|error| error.contains("the endpoint could not be reached")),
        "the report names the endpoint: {detail}"
    );

    let head = proxy.head();
    assert!(
        head.starts_with("CONNECT api.x.ai:443"),
        "the proxy is asked for the endpoint, which is not resolved here: {head}"
    );
    assert!(
        head.contains("Proxy-Authorization: Bearer proxy-token"),
        "the tunnel is authenticated as the sandbox expects: {head}"
    );
}

#[test]
fn an_agent_with_a_partial_endpoint_does_not_start() {
    // The three flags belong together, and the failure belongs at startup rather
    // than at the first task. Each of the three alone is a partial set.
    for partial in [
        vec!["--model", "grok-4.7"],
        vec!["--base-url", "https://api.x.ai/v1"],
        vec!["--env-key", "PROVIDER_KEY"],
    ] {
        let mut args = vec!["--socket", "/nonexistent/bus.sock", "--session", "s"];
        args.extend(partial.iter().copied());
        let (status, stderr) = run_agent(&args);
        assert!(
            !status.success(),
            "a partial endpoint must not start: {args:?}"
        );
        assert!(
            stderr.contains("--model, --base-url, and --env-key"),
            "stderr was: {stderr}"
        );
    }
}

#[test]
fn an_agent_whose_key_variable_is_unset_does_not_start() {
    // The report names the variable, never a value.
    let (status, stderr) = run_agent(&[
        "--socket",
        "/nonexistent/bus.sock",
        "--session",
        "s",
        "--model",
        "grok-4.7",
        "--base-url",
        "https://api.x.ai/v1",
        "--env-key",
        "A_KEY_NO_TEST_SETS",
    ]);
    assert!(!status.success(), "a missing key must not start");
    assert!(
        stderr.contains("A_KEY_NO_TEST_SETS"),
        "stderr names the variable: {stderr}"
    );
}

/// The `git` on the host's `PATH`, which the agent applies patches with.
fn host_git() -> PathBuf {
    std::env::var("PATH")
        .unwrap_or_default()
        .split(':')
        .filter(|dir| !dir.is_empty())
        .map(|dir| Path::new(dir).join("git"))
        .find(|candidate| candidate.is_file())
        .expect("`git` exists on a supported host")
}

/// Whether `event` is an output for `session` of `kind` about `action`.
fn is_output_for(
    event: &CloudEvent,
    session: &str,
    kind: &str,
    action: &str,
) -> bool {
    is_output(event, session, kind)
        && event
            .data
            .as_ref()
            .is_some_and(|data| data["action"] == json!(action))
}

#[test]
fn an_agent_reads_a_file_with_the_read_action() {
    let server = BusServer::start("read");
    let observer = server.client();
    let (handler, seen) = recorder();
    observer.subscribe(0, handler).expect("subscribes");
    let workspace = Workspace::new("read");
    std::fs::write(workspace.0.join("f.txt"), "hello from a file\n").expect("writes the file");

    let client = server.client();
    client
        .publish(incoming(
            "agent.session.command",
            "s-r",
            json!({"action": "read", "detail": {"path": "f.txt"}}),
        ))
        .expect("publishes the command");

    let _agent = Agent::start(&server, "s-r", &workspace.0, 0);

    let output = await_event(&seen, "the read's output", |event| {
        is_output_for(event, "s-r", "done", "read")
    });
    assert_eq!(output.data.as_ref().expect("data")["kind"], "done");
    assert_eq!(
        output.data.as_ref().expect("data")["detail"]["content"],
        "hello from a file\n",
        "the file's content is returned"
    );
}

#[test]
fn an_agent_finds_a_line_with_code_search() {
    let server = BusServer::start("search");
    let observer = server.client();
    let (handler, seen) = recorder();
    observer.subscribe(0, handler).expect("subscribes");
    let workspace = Workspace::new("search");
    std::fs::write(workspace.0.join("f.txt"), "alpha\nneedle here\nbeta\n")
        .expect("writes the file");

    let client = server.client();
    client
        .publish(incoming(
            "agent.session.command",
            "s-cs",
            json!({"action": "code_search", "detail": {"pattern": "needle"}}),
        ))
        .expect("publishes the command");

    let _agent = Agent::start(&server, "s-cs", &workspace.0, 0);

    let output = await_event(&seen, "the search's output", |event| {
        is_output_for(event, "s-cs", "done", "code_search")
    });
    let detail = &output.data.as_ref().expect("data")["detail"];
    assert_eq!(detail["matches"][0]["path"], "f.txt");
    assert_eq!(detail["matches"][0]["line"], 2);
    assert_eq!(detail["matches"][0]["text"], "needle here");
}

#[test]
fn an_agent_applies_a_patch_through_git() {
    let server = BusServer::start("patch");
    let observer = server.client();
    let (handler, seen) = recorder();
    observer.subscribe(0, handler).expect("subscribes");
    let workspace = Workspace::new("patch");
    std::fs::write(workspace.0.join("f.txt"), "hello\n").expect("writes the file");

    let client = server.client();
    client
        .publish(incoming(
            "agent.session.command",
            "s-pa",
            json!({
                "action": "patch",
                "detail": {
                    "diff": "--- a/f.txt\n+++ b/f.txt\n@@ -1 +1 @@\n-hello\n+world\n",
                },
            }),
        ))
        .expect("publishes the command");

    let git = host_git();
    let _agent = Agent::start_with(
        &server,
        "s-pa",
        &workspace.0,
        0,
        &["--git".to_owned(), git.to_string_lossy().into_owned()],
        &[],
    );

    let output = await_event(&seen, "the patch's output", |event| {
        is_output_for(event, "s-pa", "done", "patch")
    });
    assert_eq!(
        output.data.as_ref().expect("data")["detail"]["code"],
        0,
        "detail: {}",
        output.data.as_ref().expect("data")["detail"]
    );
    assert_eq!(
        std::fs::read_to_string(workspace.0.join("f.txt")).expect("the file is on the host"),
        "world\n",
        "the patch changed the file on disk"
    );
}

#[test]
fn a_readonly_agent_refuses_shell_and_patch_but_reads() {
    let server = BusServer::start("readonly");
    let observer = server.client();
    let (handler, seen) = recorder();
    observer.subscribe(0, handler).expect("subscribes");
    let workspace = Workspace::new("readonly");
    std::fs::write(workspace.0.join("f.txt"), "hello\n").expect("writes the file");

    let client = server.client();
    for command in [
        json!({"action": "shell", "detail": {"argv": ["/bin/sh", "-c", "echo hi"]}}),
        json!({
            "action": "patch",
            "detail": {
                "diff": "--- a/f.txt\n+++ b/f.txt\n@@ -1 +1 @@\n-hello\n+world\n",
            },
        }),
        json!({"action": "read", "detail": {"path": "f.txt"}}),
    ] {
        client
            .publish(incoming("agent.session.command", "s-ro", command))
            .expect("publishes a command");
    }

    let _agent = Agent::start_with(
        &server,
        "s-ro",
        &workspace.0,
        0,
        &["--mode".to_owned(), "readonly".to_owned()],
        &[],
    );

    let shell = await_event(&seen, "the shell refusal", |event| {
        is_output_for(event, "s-ro", "error", "shell")
    });
    assert_eq!(
        shell.data.as_ref().expect("data")["detail"]["reason"],
        "`shell` is not available in readonly mode"
    );

    let patch = await_event(&seen, "the patch refusal", |event| {
        is_output_for(event, "s-ro", "error", "patch")
    });
    assert_eq!(
        patch.data.as_ref().expect("data")["detail"]["reason"],
        "`patch` is not available in readonly mode"
    );

    let read = await_event(&seen, "the read's output", |event| {
        is_output_for(event, "s-ro", "done", "read")
    });
    assert_eq!(
        read.data.as_ref().expect("data")["detail"]["content"],
        "hello\n"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.0.join("f.txt")).expect("the file is on the host"),
        "hello\n",
        "nothing changed the file"
    );
}
