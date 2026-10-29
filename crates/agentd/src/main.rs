//! The daemon. One process, one store, one loop.
//!
//! Under systemd or launchd this process is supervised: started once,
//! restarted on exit, and expected to survive a crash with nothing more than a
//! re-fold of the log. It holds no state across steps that it could not have
//! derived from the store instead.

use std::path::Path;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use agent_model::{HttpProvider, Profile};
use agent_runtime::bus::Refusal;
use agent_runtime::cli::{ArgError, Args, USAGE, parse};
use agent_runtime::config;
use agent_runtime::{Reactor, Step, clock, route};

/// How long to wait before looking for work again when there was none.
const IDLE_WAIT: Duration = Duration::from_millis(250);

fn reactor_from_config(path: &Path) -> Result<Reactor, String> {
    let config =
        config::load(path, &|name| std::env::var(name).ok()).map_err(|error| error.to_string())?;
    // Before the store is opened, and before the first tool call: a host that
    // will not give the launcher its namespaces should say so while somebody is
    // still watching the start, not in a failed call at three in the morning.
    agent_runtime::confine::probe(&config.settings)?;
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
    bus_path: &Path,
) -> Result<(), String> {
    let bus = agent_runtime::bus::Bus::bind(bus_path)
        .map_err(|error| format!("{}: {error}", bus_path.display()))?;
    reactor
        .start(clock::now().map_err(|error| error.to_string())?)
        .map_err(|error| error.to_string())?;
    let supervisor = std::env::var_os("NOTIFY_SOCKET");
    ping(supervisor.as_deref(), agent_runtime::notify::READY);
    // A tool call is one loop iteration, so a long one would outlive the watchdog
    // and systemd would restart the daemon in the middle of the effect.
    let heartbeat_socket = supervisor.clone();
    let mut last = Instant::now();
    reactor.set_heartbeat(Box::new(move || {
        if last.elapsed() >= Duration::from_secs(5) {
            ping(heartbeat_socket.as_deref(), agent_runtime::notify::WATCHDOG);
            last = Instant::now();
        }
    }));
    loop {
        let now = clock::now().map_err(|error| error.to_string())?;
        // The socket is read before the step: a command that arrived while the
        // loop was sleeping belongs in the turn this iteration is about to make.
        // Each client is answered after the command has been written, and a
        // storage failure exits after that answer so the client is not left
        // holding a connection the daemon will never close.
        for pending in bus.poll() {
            let reply = match pending.command() {
                Ok(command) => route::route(&mut reactor, command, now),
                Err(refusal) => Err(refusal.clone()),
            };
            let fatal = reply
                .as_ref()
                .err()
                .filter(|refusal| matches!(refusal, Refusal::Storage(_) | Refusal::Corrupt(_)))
                .cloned();
            pending.answer(&reply);
            if let Some(refusal) = fatal {
                return Err(refusal.message());
            }
        }
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
    // A tool runs as the same user, and a dumpable daemon lets it read the key
    // from /proc. Hide the process before anything opens the store.
    agent_runtime::confine::hide_from_tools()?;
    let state_dir = match (&args.config, &args.state_dir) {
        (Some(path), _) => config_state_dir(path)?,
        (None, Some(dir)) => dir.clone(),
        (None, None) => return Err("no state was named".to_string()),
    };
    std::fs::create_dir_all(&state_dir).map_err(|error| error.to_string())?;

    // Before the store is opened, and before the socket is bound. Two daemons on
    // one state directory each append, each fold, and each decide what to do
    // about the same turn; SQLite serialises the writes rather than refusing them,
    // so the log stays valid and its meaning does not. The second one leaves.
    let _lock = agent_runtime::lock::Lock::take(&state_dir).map_err(|error| error.to_string())?;

    let reactor = match &args.config {
        Some(path) => reactor_from_config(path)?,
        None => Reactor::open(&state_dir.join("agent.db")).map_err(|error| error.to_string())?,
    };
    let bus_path = args
        .bus
        .clone()
        .unwrap_or_else(|| state_dir.join(agent_runtime::bus::SOCKET));
    drive(reactor, args.once, &bus_path)
}

/// Where a configured daemon keeps its store, which is also where its socket goes.
fn config_state_dir(path: &Path) -> Result<std::path::PathBuf, String> {
    agent_runtime::config::load(path, &|name| std::env::var(name).ok())
        .map(|config| config.state_dir)
        .map_err(|error| error.to_string())
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
