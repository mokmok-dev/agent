//! Drives a real ACP agent with the `AcpProtocol` bridge, optionally inside the
//! sandbox with the managed egress proxy, and prints the events it produces.
//!
//! Run it unconfined (default), for a host without the Landlock helper:
//!
//! ```sh
//! cargo run -p agentd --features sandbox --example acp_handshake -- \
//!     opencode2 acp
//! ```
//!
//! Or confined:
//!
//! ```sh
//! cargo run -p agentd --features sandbox --example acp_handshake -- \
//!     --sandbox -- opencode2 acp
//! ```
//!
//! A confined agent binds its own loopback HTTP server; with no proxy that is a
//! private network namespace (loopback, no egress). Add `--egress host:port` to
//! reach a model provider through the daemon's CONNECT proxy *and* keep the
//! agent's own loopback server:
//!
//! ```sh
//! cargo run -p agentd --features sandbox --example acp_handshake -- \
//!     --sandbox --egress openrouter.ai:443 -- opencode2 acp
//! ```
//!
//! That combined model is the shared network with the proxy port and the
//! ephemeral range open (see `docs/egress.md`); the injected `NO_PROXY` keeps the
//! agent's own loopback off the tunnel.

use agentd::proxy::{Egress, Proxy};
use agentd::session::{SessionManager, Supervision};
use agentd_events::Event;
use agentd_sandbox::{
    Access, ExecResult, Executor, FsEntry, FsPolicy, HostPort, Policy, Sandbox, ShellPolicy,
    SpawnError,
};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

/// An executor that spawns the command unconfined, for the default mode.
struct PlainExecutor {
    workdir: PathBuf,
    /// The shell to run commands with; `/bin/bash` does not exist on some
    /// distributions (e.g. NixOS), so it is resolved on this host.
    shell: PathBuf,
}

#[async_trait]
impl Executor for PlainExecutor {
    async fn exec(
        &self,
        command: &str,
    ) -> ExecResult {
        let output = std::process::Command::new(&self.shell)
            .arg("-c")
            .arg(command)
            .current_dir(&self.workdir)
            .output();
        plain_result(output)
    }

    async fn spawn(
        &self,
        command: &str,
    ) -> Result<tokio::process::Child, SpawnError> {
        let child = tokio::process::Command::new(&self.shell)
            .arg("-c")
            .arg(command)
            .current_dir(&self.workdir)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .process_group(0)
            .spawn()?;
        Ok(child)
    }
}

/// Maps a command's output to an [`ExecResult`].
fn plain_result(output: Result<std::process::Output, std::io::Error>) -> ExecResult {
    match output {
        Ok(output) => ExecResult {
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            exit_code: output.status.code().unwrap_or(1),
            denied: false,
        },
        Err(error) => ExecResult {
            stdout: String::new(),
            stderr: error.to_string(),
            exit_code: 126,
            denied: false,
        },
    }
}

/// The parsed command line.
#[derive(Debug)]
struct Args {
    sandboxed: bool,
    egress: Vec<HostPort>,
    /// A prompt to send once the session is ready, to exercise a model call.
    prompt: Option<String>,
    command: String,
}

/// Parses the command line.
fn parse_args() -> Result<Args, Box<dyn std::error::Error>> {
    let mut sandboxed = false;
    let mut egress: Vec<HostPort> = Vec::new();
    let mut prompt = None;
    let mut command_args: Vec<String> = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--sandbox" => sandboxed = true,
            "--egress" => {
                let value = args.next().ok_or("--egress needs host:port")?;
                egress.push(parse_host_port(&value)?);
            },
            "--prompt" => prompt = Some(args.next().ok_or("--prompt needs text")?),
            _ => command_args.push(arg),
        }
    }
    if command_args.is_empty() {
        return Err(
            "usage: acp_handshake [--sandbox] [--egress host:port] [--prompt text] <command> [args...]"
                .into(),
        );
    }
    Ok(Args {
        sandboxed,
        egress,
        prompt,
        command: command_args.join(" "),
    })
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let Args {
        sandboxed,
        egress,
        prompt,
        command,
    } = parse_args()?;
    let workdir = std::env::current_dir()?;

    let events_path =
        std::env::temp_dir().join(format!("acp-handshake-{}.jsonl", std::process::id()));
    let log = agentd_events::EventLog::open(&events_path)?;
    let subscriber = log.subscribe();

    let mut policy = Policy {
        fs: FsPolicy {
            entries: vec![FsEntry {
                path: workdir.clone(),
                access: Access::Write,
            }],
            ..FsPolicy::default()
        },
        shell: ShellPolicy {
            workdir: workdir.clone(),
            ..ShellPolicy::default()
        },
        ..Policy::default()
    };
    let proxy = configure_egress(&mut policy, egress, sandboxed).await?;

    let sandbox = if sandboxed {
        // An ACP agent binds its own loopback HTTP server and talks to it. With
        // no proxy that is a private network namespace (loopback, no egress);
        // with a proxy the network is shared and the ephemeral range is opened
        // alongside the proxy port (see `docs/egress.md`).
        policy.network.loopback = true;
        Arc::new(Sandbox::new(&policy, log.clone(), "acp-demo")?)
    } else {
        let shell = std::env::var_os("SHELL")
            .map(PathBuf::from)
            .filter(|path| path.is_file())
            .unwrap_or_else(|| PathBuf::from("/bin/bash"));
        let executor = Arc::new(PlainExecutor {
            workdir: workdir.clone(),
            shell,
        });
        Arc::new(Sandbox::with_executor(
            &policy,
            log.clone(),
            "acp-demo",
            executor,
        )?)
    };

    let manager = SessionManager::with_sandbox(log.clone(), sandbox, command, "acp-demo")
        .with_protocol(Arc::new(agentd::bridge::AcpProtocol::default()))
        .with_workdir(workdir)
        .with_supervision(Supervision {
            max_restarts: 0,
            restart_backoff: Duration::from_secs(1),
            lifetime: Some(Duration::from_secs(120)),
        });

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let run = tokio::spawn(async move {
        let _ = manager.run(shutdown_rx).await;
    });

    log.publish(Event::new(
        "session.requested",
        json!({ "session_id": "opencode-demo" }),
    ))
    .await?;

    let mut events = subscriber;
    let ready = watch_until_ready(&mut events).await;

    // With a prompt, send it once ready and wait for the turn to finish, which
    // is where a model call happens (through the proxy, if one is configured).
    let turn = if ready && let Some(prompt) = prompt {
        log.publish(
            agentd::bridge::prompt(&json!([{ "type": "text", "text": prompt }]))
                .with_subject("session:opencode-demo"),
        )
        .await?;
        watch_for_turn(&mut events).await
    } else {
        None
    };

    let _ = shutdown_tx.send(true);
    let _ = run.await;
    if let Some(proxy) = proxy {
        proxy.stop();
    }
    let _ = std::fs::remove_file(&events_path);

    match (ready, turn) {
        (true, None) => {
            println!("ACP handshake completed: the agent answered initialize and session/new.");
            Ok(())
        },
        (true, Some(stop_reason)) => {
            println!("ACP turn completed with stop reason: {stop_reason}");
            Ok(())
        },
        (false, _) => Err("the ACP handshake did not complete".into()),
    }
}

/// Starts the daemon's CONNECT proxy for `egress` and points `policy` at it.
///
/// [`Proxy::start_for_policy`] picks the transport the host supports (a Unix
/// socket inside a private network namespace, or loopback TCP without one) and
/// injects the proxy env and `NO_PROXY`. Egress is a confined-only concern: an
/// unconfined run has no sandbox to reach the network through, so it needs no
/// proxy and `--egress` is ignored there. Returns the running proxy, or `None`
/// when there is none to start.
async fn configure_egress(
    policy: &mut Policy,
    egress: Vec<HostPort>,
    sandboxed: bool,
) -> Result<Option<Proxy>, Box<dyn std::error::Error>> {
    if !sandboxed || egress.is_empty() {
        if !sandboxed && !egress.is_empty() {
            eprintln!(
                "--egress only applies with --sandbox; an unconfined run reaches the network directly"
            );
        }
        return Ok(None);
    }
    Ok(Some(
        Proxy::start_for_policy(policy, Egress::new(egress)).await?,
    ))
}

/// Prints session events until the handshake completes or times out.
async fn watch_until_ready(
    events: &mut tokio::sync::broadcast::Receiver<agentd_events::LogEntry>
) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_secs(5), events.recv()).await {
            Ok(Ok(entry)) => {
                let event = entry.event;
                if event.r#type.starts_with("session.") || event.r#type.starts_with("sandbox.") {
                    println!("[{}] {}", event.r#type, summarize(&event));
                }
                if event.r#type == "session.acp.ready" {
                    return true;
                }
                if event.r#type == "session.acp.failed" || event.r#type == "session.exited" {
                    return false;
                }
            },
            Ok(Err(_)) | Err(_) => return false,
        }
    }
    false
}

/// Prints session events until a prompt turn ends, returning its stop reason.
async fn watch_for_turn(
    events: &mut tokio::sync::broadcast::Receiver<agentd_events::LogEntry>
) -> Option<String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_secs(5), events.recv()).await {
            Ok(Ok(entry)) => {
                let event = entry.event;
                if event.r#type.starts_with("session.") || event.r#type.starts_with("sandbox.") {
                    println!("[{}] {}", event.r#type, summarize(&event));
                }
                if event.r#type == agentd::bridge::ACP_TURN_COMPLETED {
                    return Some(
                        event.data["stop_reason"]
                            .as_str()
                            .unwrap_or("unknown")
                            .to_string(),
                    );
                }
                if event.r#type == agentd::bridge::ACP_FAILED || event.r#type == "session.exited" {
                    return None;
                }
            },
            Ok(Err(_)) | Err(_) => return None,
        }
    }
    None
}

/// Parses `host:port`, accepting a bracketed IPv6 literal.
fn parse_host_port(value: &str) -> Result<HostPort, Box<dyn std::error::Error>> {
    let (host, port) = value.rsplit_once(':').ok_or("expected host:port")?;
    Ok(HostPort {
        host: host.trim_matches(['[', ']']).to_string(),
        port: port.parse()?,
    })
}

/// A short, log-friendly rendering of an event's payload.
fn summarize(event: &Event) -> String {
    let data: &Value = &event.data;
    match event.r#type.as_str() {
        "session.acp.ready" => String::from("session is ready"),
        "session.bridge.inbound" => {
            let message = &data["message"];
            if let Some(update) = message
                .get("params")
                .and_then(|params| params.get("update"))
            {
                // A session/update notification: surface the text the agent
                // streams, so the model's output is visible.
                if let Some(text) = update
                    .get("content")
                    .and_then(|content| content.get("text"))
                    .and_then(Value::as_str)
                {
                    let kind = update
                        .get("sessionUpdate")
                        .and_then(Value::as_str)
                        .unwrap_or("update");
                    return format!("{kind}: {}", truncate(text, 200));
                }
            }
            message.get("method").and_then(Value::as_str).map_or_else(
                || {
                    message.get("result").map_or_else(
                        || String::from("agent message"),
                        |result| format!("agent response: {}", truncate(&result.to_string(), 120)),
                    )
                },
                |method| format!("agent -> client: {method}"),
            )
        },
        "session.started" => format!("session started ({})", data["session_id"]),
        "session.exited" => format!("exited: {data}"),
        _ => truncate(&data.to_string(), 120),
    }
}

/// Truncates `text` to at most `max` characters.
fn truncate(
    text: &str,
    max: usize,
) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut shortened: String = text.chars().take(max).collect();
    shortened.push('…');
    shortened
}
