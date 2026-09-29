//! The agent binary.
//!
//! argv: `agent-agent --socket <bus.sock> --session <id> [--workdir <dir>]`.
//!
//! The capability dispatches by action: `shell` runs a command in the session's
//! workspace, and anything else is reported as unknown, so a client and an agent
//! of different versions do not crash each other.

use std::path::PathBuf;

use clap::Parser;
use serde_json::json;

use agentd_agent::contract::{Command, Output, OutputKind};
use agentd_agent::loopcore::{self, AgentConfig, Capability, Reporter};
use agentd_agent::shell::{ACTION as SHELL, Shell};

/// The purpose-built agent for one session.
#[derive(Debug, Parser)]
#[command(name = "agent-agent", about = "Run one session's agent")]
struct Cli {
    /// The bus's Unix socket.
    #[arg(long)]
    socket: PathBuf,
    /// The session this agent acts for.
    #[arg(long)]
    session: String,
    /// The sequence to replay from, or 0 for the whole log.
    #[arg(long, default_value_t = 0)]
    from_seq: u64,
    /// The working directory for a `shell` action. Defaults to the session's
    /// workspace, which the session policy has bound read-write.
    #[arg(long)]
    workdir: Option<PathBuf>,
}

/// The dispatch: one capability per action name.
#[derive(Debug)]
enum Dispatch {
    Shell(Shell),
}

impl Capability for Dispatch {
    fn act(
        &self,
        command: &Command,
        reporter: &dyn Reporter,
    ) -> Output {
        match self {
            Self::Shell(shell) => {
                if command.action == SHELL {
                    return shell.act(command, reporter);
                }
                unknown(command)
            },
        }
    }
}

/// The output for an action no capability answers.
fn unknown(command: &Command) -> Output {
    Output {
        kind: OutputKind::Error,
        action: command.action.clone(),
        detail: json!({"reason": "the agent has no capability for this action"}),
    }
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let workdir = cli.workdir.unwrap_or_else(|| PathBuf::from("."));
    let capability = Dispatch::Shell(Shell::in_workspace(workdir));
    let config = AgentConfig {
        bus_socket: cli.socket,
        session: cli.session,
    };
    loopcore::run(&config, &capability, cli.from_seq)?;
    Ok(())
}
