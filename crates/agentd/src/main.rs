//! The daemon. One process, one store, one loop.
//!
//! Under systemd or launchd this process is supervised: started once,
//! restarted on exit, and expected to survive a crash with nothing more than a
//! re-fold of the log. It holds no state across steps that it could not have
//! derived from the store instead.

use std::process::ExitCode;
use std::time::Duration;

use agent_runtime::cli::{ArgError, Args, USAGE, parse};
use agent_runtime::clock;
use agent_runtime::{Reactor, Step};

/// How long to wait before looking for work again when there was none.
const IDLE_WAIT: Duration = Duration::from_millis(250);

fn run(args: &Args) -> Result<(), String> {
    std::fs::create_dir_all(&args.state_dir).map_err(|error| error.to_string())?;
    let mut reactor =
        Reactor::open(&args.state_dir.join("agent.db")).map_err(|error| error.to_string())?;
    reactor
        .start(clock::now().map_err(|error| error.to_string())?)
        .map_err(|error| error.to_string())?;
    loop {
        let now = clock::now().map_err(|error| error.to_string())?;
        match reactor.step(now).map_err(|error| error.to_string())? {
            Step::Append(_) | Step::RequestModel { .. } => {},
            Step::Idle => {
                if args.once {
                    return Ok(());
                }
                std::thread::sleep(IDLE_WAIT);
            },
            Step::Wait { until } => {
                if args.once {
                    return Ok(());
                }
                std::thread::sleep(agent_runtime::wait_for(now, until));
            },
        }
    }
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
