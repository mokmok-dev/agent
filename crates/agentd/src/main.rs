//! The daemon. One process, one store, one loop.
//!
//! Under systemd or launchd this process is supervised: started once,
//! restarted on exit, and expected to survive a crash with nothing more than a
//! re-fold of the log. It holds no state across steps that it could not have
//! derived from the store instead.

use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use agent_model::{HttpProvider, Profile};
use agent_runtime::cli::{ArgError, Args, USAGE, parse};
use agent_runtime::config;
use agent_runtime::{Reactor, Step, clock};

/// How long to wait before looking for work again when there was none.
const IDLE_WAIT: Duration = Duration::from_millis(250);

fn reactor_from_config(path: &Path) -> Result<Reactor, String> {
    let config =
        config::load(path, &|name| std::env::var(name).ok()).map_err(|error| error.to_string())?;
    let profile = Profile {
        id: config.settings.model.clone(),
        context_window: 0,
        caches_prefixes: true,
        cache_ttl: None,
        reports_cached_tokens: true,
    };
    let provider = HttpProvider::new(
        profile,
        &config.base_url,
        config.api_key.clone(),
        config.timeout,
    );
    Reactor::with_model(
        &config.state_dir.join("agent.db"),
        Box::new(provider),
        config.settings,
    )
    .map_err(|error| error.to_string())
}

fn drive(
    mut reactor: Reactor,
    once: bool,
) -> Result<(), String> {
    reactor
        .start(clock::now().map_err(|error| error.to_string())?)
        .map_err(|error| error.to_string())?;
    let supervisor = std::env::var_os("NOTIFY_SOCKET");
    ping(supervisor.as_deref(), agent_runtime::notify::READY);
    loop {
        let now = clock::now().map_err(|error| error.to_string())?;
        match reactor.step(now).map_err(|error| error.to_string())? {
            Step::Append(_) | Step::Execute { .. } | Step::RequestModel { .. } => {},
            Step::Idle => {
                if once {
                    return Ok(());
                }
                std::thread::sleep(IDLE_WAIT);
            },
            Step::Wait { until } => {
                if once {
                    return Ok(());
                }
                std::thread::sleep(agent_runtime::wait_for(now, until));
            },
        }
        ping(supervisor.as_deref(), agent_runtime::notify::WATCHDOG);
    }
}

/// Tells the supervisor what happened, and does not crash when it is not there.
///
/// A supervisor that has gone away is worth a line, not an exit: the next
/// iteration may still be the one that finishes the work.
fn ping(
    socket: Option<&std::ffi::OsStr>,
    message: &str,
) {
    if let Err(error) = agent_runtime::notify::notify(socket, message) {
        eprintln!("agentd: cannot notify the supervisor: {error}");
    }
}

fn run(args: &Args) -> Result<(), String> {
    let reactor = match (&args.config, &args.state_dir) {
        (Some(path), _) => reactor_from_config(path)?,
        (None, Some(dir)) => {
            std::fs::create_dir_all(dir).map_err(|error| error.to_string())?;
            Reactor::open(&dir.join("agent.db")).map_err(|error| error.to_string())?
        },
        (None, None) => return Err("no state was named".to_string()),
    };
    drive(reactor, args.once)
}

fn main() -> ExitCode {
    match parse(std::env::args().skip(1)) {
        Ok(args) => match run(&args) {
            Ok(()) => ExitCode::SUCCESS,
            Err(message) => {
                eprintln!("agentd: {message}");
                ExitCode::FAILURE
            },
        },
        Err(ArgError::Help) => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        },
        Err(error) => {
            eprintln!("agentd: {error}\n{USAGE}");
            ExitCode::FAILURE
        },
    }
}
