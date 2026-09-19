//! Drives a real ACP agent with the `AcpProtocol` bridge and prints the events
//! it produces, to prove the bridge starts an ACP agent end to end.
//!
//! Run with:
//!
//! ```sh
//! cargo run -p agentd --features sandbox --example acp_handshake -- \
//!     opencode2 acp
//! ```
//!
//! The agent is run through an unconfined executor on purpose: this example is
//! about the bridge, and an ACP agent that reaches a model provider needs
//! network egress the sandbox denies (see `docs/acp.md`). Confinement is a
//! separate concern.

use agentd::session::{SessionManager, Supervision};
use agentd_events::Event;
use agentd_sandbox::{ExecResult, Executor, FsPolicy, Policy, ShellPolicy, SpawnError};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

/// An executor that spawns the command unconfined through `/bin/bash`.
struct PlainExecutor {
    workdir: PathBuf,
    /// The shell to run commands with; `/bin/bash` does not exist on some
    /// distributions (e.g. NixOS), so it is resolved from the environment.
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

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let workdir = std::env::current_dir()?;
    if args.is_empty() {
        return Err("usage: acp_handshake <command> [args...]".into());
    }
    let command = args.join(" ");

    let events_path =
        std::env::temp_dir().join(format!("acp-handshake-{}.jsonl", std::process::id()));
    let log = agentd_events::EventLog::open(&events_path)?;
    let subscriber = log.subscribe();

    let policy = Policy {
        fs: FsPolicy {
            entries: vec![agentd_sandbox::FsEntry {
                path: workdir.clone(),
                access: agentd_sandbox::Access::Write,
            }],
            ..FsPolicy::default()
        },
        shell: ShellPolicy {
            workdir: workdir.clone(),
            ..ShellPolicy::default()
        },
        ..Policy::default()
    };
    let shell = std::env::var_os("SHELL")
        .map(PathBuf::from)
        .filter(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from("/bin/bash"));
    let executor = Arc::new(PlainExecutor {
        workdir: workdir.clone(),
        shell,
    });
    let sandbox = Arc::new(agentd_sandbox::Sandbox::with_executor(
        &policy,
        log.clone(),
        "acp-demo",
        executor,
    )?);
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

    let mut ready = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    let mut events = subscriber;
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_secs(5), events.recv()).await {
            Ok(Ok(entry)) => {
                let event = entry.event;
                let interesting =
                    event.r#type.starts_with("session.") || event.r#type.starts_with("sandbox.");
                if interesting {
                    let summary = summarize(&event);
                    println!("[{}] {}", event.r#type, summary);
                }
                if event.r#type == "session.acp.ready" {
                    ready = true;
                    break;
                }
                if event.r#type == "session.acp.failed" || event.r#type == "session.exited" {
                    break;
                }
            },
            Ok(Err(_)) | Err(_) => break,
        }
    }

    let _ = shutdown_tx.send(true);
    let _ = run.await;
    let _ = std::fs::remove_file(&events_path);

    if ready {
        println!("ACP handshake completed: opencode2 answered initialize and session/new.");
        Ok(())
    } else {
        Err("the ACP handshake did not complete".into())
    }
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
        "session.started" => format!("pid session started ({})", data["session_id"]),
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
