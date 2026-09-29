//! The agent binary.
//!
//! argv: `agent-agent --socket <bus.sock> --session <id>`.
//!
//! The binary's own capability is `echo`: it reports what it was asked and marks
//! it done. That is the seam a real capability replaces, and it is what proves
//! the loop end to end.

use std::path::PathBuf;

use clap::Parser;
use serde_json::json;

use agentd_agent::contract::{Command, Output, OutputKind};
use agentd_agent::loopcore::{self, AgentConfig, Capability};

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
}

/// The binary's capability: report the command and finish.
#[derive(Debug)]
struct Echo;

impl Capability for Echo {
    fn act(
        &self,
        command: &Command,
    ) -> Output {
        Output {
            kind: OutputKind::Done,
            action: command.action.clone(),
            detail: json!({"echoed": command.detail}),
        }
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

    let config = AgentConfig {
        bus_socket: cli.socket,
        session: cli.session,
    };
    loopcore::run(&config, &Echo, cli.from_seq)?;
    Ok(())
}
