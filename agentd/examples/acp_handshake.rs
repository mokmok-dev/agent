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
//! Or confined. A confined agent needs free loopback for its own HTTP server,
//! which is a private network namespace with no egress, so `--sandbox` cannot
//! be combined with `--egress`:
//!
//! ```sh
//! cargo run -p agentd --features sandbox --example acp_handshake -- \
//!     --sandbox -- opencode2 acp
//! ```
//!
//! The confined run proves the sandbox can start an ACP agent under a private
//! namespace. Reaching a remote model is the separate proxy model
//! (`--egress`, unconfined here), which additionally needs the agent to honour
//! the injected `HTTP_PROXY` and hold credentials (see `docs/egress.md`).

use agentd::proxy::{Egress, Proxy};
use agentd::session::{SessionManager, Supervision};
use agentd_events::Event;
use agentd_sandbox::{
    Access, EnvVar, ExecResult, Executor, FsEntry, FsPolicy, HostPort, Policy, Sandbox,
    ShellPolicy, SpawnError,
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
    command: String,
}

/// Parses the command line.
fn parse_args() -> Result<Args, Box<dyn std::error::Error>> {
    let mut sandboxed = false;
    let mut egress: Vec<HostPort> = Vec::new();
    let mut command_args: Vec<String> = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--sandbox" => sandboxed = true,
            "--egress" => {
                let value = args.next().ok_or("--egress needs host:port")?;
                egress.push(parse_host_port(&value)?);
            },
            _ => command_args.push(arg),
        }
    }
    if command_args.is_empty() {
        return Err(
            "usage: acp_handshake [--sandbox] [--egress host:port] <command> [args...]".into(),
        );
    }
    Ok(Args {
        sandboxed,
        egress,
        command: command_args.join(" "),
    })
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let Args {
        sandboxed,
        egress,
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

    // A confined ACP agent needs loopback for its own HTTP server, but the
    // daemon proxy lives on the host's loopback, which a private namespace
    // cannot reach. The two models are mutually exclusive, so this combination
    // is not a thing the sandbox can express (see `docs/egress.md`).
    if sandboxed && !egress.is_empty() {
        return Err(
            "an ACP agent needs loopback, which is a private namespace with no route to the \
             host's proxy; --sandbox and --egress cannot be combined"
                .into(),
        );
    }

    // The daemon's CONNECT proxy: the only egress path the sandbox grants.
    let proxy = if egress.is_empty() {
        None
    } else {
        let proxy = Proxy::start(Egress::new(egress.clone())).await?;
        for name in ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"] {
            policy.shell.env.push(EnvVar {
                name: String::from(name),
                value: proxy.url(),
            });
        }
        policy.network.proxy = Some(agentd_sandbox::Proxy {
            port: proxy.address().port(),
            egress,
        });
        Some(proxy)
    };

    let sandbox = if sandboxed {
        // An ACP agent binds its own loopback HTTP server and talks to it; a
        // private network namespace gives it free loopback with no egress.
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

    let ready = watch_until_ready(subscriber).await;

    let _ = shutdown_tx.send(true);
    let _ = run.await;
    if let Some(proxy) = proxy {
        proxy.stop();
    }
    let _ = std::fs::remove_file(&events_path);

    if ready {
        println!("ACP handshake completed: the agent answered initialize and session/new.");
        Ok(())
    } else {
        Err("the ACP handshake did not complete".into())
    }
}

/// Prints the session events until the ACP handshake completes or times out.
async fn watch_until_ready(
    mut events: tokio::sync::broadcast::Receiver<agentd_events::LogEntry>
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
