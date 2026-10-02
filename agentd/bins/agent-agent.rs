//! The agent binary.
//!
//! argv: `agent-agent --socket <bus.sock> --session <id> [--workdir <dir>]
//! [--mode readonly|readwrite]`, plus
//! `--model <id> --base-url <url> --env-key <NAME>` when the session's settings
//! name a model endpoint.
//!
//! The capability dispatches by action: `mode` reports or switches the session's
//! mode, the coding belt answers `read`, `code_search`, `patch`, and `shell` in
//! the session's workspace, `task` drives the model when an endpoint was given,
//! and anything else is reported as unknown, so a client and an agent of different
//! versions do not crash each other.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, anyhow};
use clap::Parser;

use agentd_agent::coding::{ACTION_MODE as MODE, Coding, Mode, act_mode};
use agentd_agent::contract::{Command, Output};
use agentd_agent::loopcore::{self, AgentConfig, Capability, Reporter};
use agentd_agent::openai::{self, Client, Config as ModelConfig};
use agentd_agent::task::{ACTION as TASK, Task};
use zeroize::Zeroizing;

/// The variables an HTTP client reads to find its proxy, in the order it reads
/// them. The sandbox sets all three to the same value.
const PROXY_ENV: [&str; 3] = ["HTTPS_PROXY", "ALL_PROXY", "HTTP_PROXY"];

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
    /// The capability set for this session, `readonly` or `readwrite`.
    #[arg(long, default_value = "readwrite")]
    mode: Mode,
    /// The model id the endpoint is asked for, with `--base-url` and `--env-key`.
    #[arg(long)]
    model: Option<String>,
    /// The endpoint's base URL, with `--model` and `--env-key`.
    #[arg(long)]
    base_url: Option<String>,
    /// The environment variable holding the API key, with `--model` and
    /// `--base-url`.
    #[arg(long)]
    env_key: Option<String>,
}

/// The dispatch: the coding belt, and the model-driven `task` when it exists.
#[derive(Debug)]
struct Dispatch {
    /// The one belt the session runs under, shared with the task so a mode
    /// switched here is the mode the task reads.
    coding: Arc<Coding>,
    /// The model-driven `task` capability, when the session's settings named an
    /// endpoint. Without one, `task` is an unknown action like any other.
    task: Option<Task>,
}

impl Capability for Dispatch {
    fn act(
        &self,
        command: &Command,
        reporter: &dyn Reporter,
    ) -> Output {
        // The mode command is a session command: it is answered here, before the
        // task and the belt, and it is not a tool the model can call.
        if command.action == MODE {
            return act_mode(&self.coding, &command.detail);
        }
        if command.action == TASK
            && let Some(task) = &self.task
        {
            return task.act(command, reporter);
        }
        self.coding.act(command, reporter)
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

    let workdir = cli.workdir.clone().unwrap_or_else(|| PathBuf::from("."));
    let coding =
        Arc::new(Coding::new(&workdir, cli.mode).context("the workspace root is not usable")?);
    let capability = Dispatch {
        coding: Arc::clone(&coding),
        task: task(&cli, coding)?,
    };
    let config = AgentConfig {
        bus_socket: cli.socket,
        session: cli.session,
    };
    loopcore::run(&config, &capability, cli.from_seq)?;
    Ok(())
}

/// The model-driven capability, when the flags name an endpoint.
///
/// The three flags belong together: an endpoint without a model, or a model with
/// no way to reach it, would fail on the first task instead of here, where an
/// operator can see it.
fn task(
    cli: &Cli,
    coding: Arc<Coding>,
) -> anyhow::Result<Option<Task>> {
    let (Some(model), Some(base_url), Some(env_key)) = (&cli.model, &cli.base_url, &cli.env_key)
    else {
        if cli.model.is_some() || cli.base_url.is_some() || cli.env_key.is_some() {
            return Err(anyhow!(
                "--model, --base-url, and --env-key are given together"
            ));
        }
        return Ok(None);
    };

    // The value is read here and never logged: the report a failed call produces
    // names the variable, not the key. The string the environment gives back is
    // moved into the client, which holds it as a secret that is wiped when it drops.
    let api_key = std::env::var(env_key)
        .map_err(|_| anyhow!("`{env_key}` is not set, so this agent has no key"))?;
    let mut config = ModelConfig::new(base_url.clone(), model.clone(), api_key);
    if let Some(proxy) = proxy()? {
        config = config.with_proxy(proxy);
    }
    let client = Client::new(config).context("the model endpoint is not usable")?;
    Ok(Some(Task::new(Box::new(client), coding)))
}

/// The proxy to reach the endpoint through, from the environment.
///
/// The sandbox sets all three names to the same value when it grants egress, and
/// that value carries the proxy's token. A process with none of them has no proxy
/// configured, which is when connecting directly is right. `NO_PROXY` is
/// deliberately not read: it names the command's own loopback, which is never the
/// endpoint.
///
/// The URL is wiped once it has been parsed. The environment block itself keeps its
/// own copy, which this process cannot scrub: `std::env::remove_var` is `unsafe` in
/// edition 2024, and this workspace denies `unsafe` everywhere.
fn proxy() -> anyhow::Result<Option<openai::Proxy>> {
    for name in PROXY_ENV {
        if let Ok(url) = std::env::var(name) {
            let url = Zeroizing::new(url);
            return Ok(Some(openai::Proxy::parse(&url)?));
        }
    }
    Ok(None)
}
