//! `agentctl`. Reads arguments, runs one command, prints one line.
//!
//! Every decision this binary makes lives in the library beside it, so this
//! file is wiring: it resolves the state directory, opens the store, and turns
//! a returned line or failure into an exit code.

use std::path::PathBuf;
use std::process::ExitCode;

use agent_store::Store;
use agentctl::{Failure, Intent, Invocation, Parsed, exit, parse, report};

fn main() -> ExitCode {
    let json = std::env::args().any(|argument| argument == "--json");
    match parse(std::env::args_os()) {
        Ok(Parsed::Text(text)) => {
            print!("{}", text.trim_end());
            println!();
            ExitCode::SUCCESS
        },
        Ok(Parsed::Invocation(invocation)) => run(&invocation),
        Err(failure) => report_failure(&failure, json),
    }
}

fn run(invocation: &Invocation) -> ExitCode {
    match dispatch(invocation) {
        Ok(line) => {
            println!("{line}");
            ExitCode::SUCCESS
        },
        Err(failure) => report_failure(&failure, invocation.json),
    }
}

fn dispatch(invocation: &Invocation) -> Result<String, Failure> {
    match &invocation.intent {
        Intent::Version => Ok(if invocation.json {
            report::version_json()
        } else {
            report::version_text()
        }),
        Intent::Verify { .. } => verify(invocation),
        Intent::Status => status(invocation),
        Intent::Fork { .. }
        | Intent::Send { .. }
        | Intent::Approve { .. }
        | Intent::Deny { .. } => bus(invocation),
        other => Err(Failure::unsupported(other.name(), other.needs())),
    }
}

fn verify(invocation: &Invocation) -> Result<String, Failure> {
    let path = store_path(invocation)?;
    let store = Store::open_read_only(&path).map_err(|error| report::store_failure(&error))?;
    let found = store
        .verify()
        .map_err(|error| report::store_failure(&error))?;
    Ok(if invocation.json {
        report::verify_json(&found)
    } else {
        report::verify_text(&found)
    })
}

fn store_path(invocation: &Invocation) -> Result<PathBuf, Failure> {
    invocation
        .state_dir
        .clone()
        .or_else(|| std::env::var_os("STATE_DIRECTORY").map(PathBuf::from))
        .or_else(default_dir)
        .map(|dir| dir.join("agent.db"))
        .ok_or_else(|| {
            Failure::config("no state directory was named and none could be found".to_string())
        })
}

#[cfg(target_os = "macos")]
fn default_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| {
        PathBuf::from(&home)
            .join("Library")
            .join("Application Support")
            .join("agent")
    })
}

#[cfg(not(target_os = "macos"))]
const fn default_dir() -> Option<PathBuf> {
    None
}

fn report_failure(
    failure: &Failure,
    json: bool,
) -> ExitCode {
    eprintln!("{}", failure.render(json));
    ExitCode::from(failure.exit.code())
}

/// One line to the daemon's socket, and its answer.
fn bus(invocation: &Invocation) -> Result<String, Failure> {
    use std::io::{BufRead as _, BufReader, Write as _};

    let intent = &invocation.intent;
    let line = intent
        .request_line()
        .ok_or_else(|| Failure::unsupported(intent.name(), intent.needs()))?;
    let socket = store_path(invocation)?
        .parent()
        .map(|dir| dir.join("bus.sock"))
        .ok_or_else(|| Failure::config("no state directory was named".to_string()))?;
    let stream = std::os::unix::net::UnixStream::connect(&socket).map_err(|error| {
        Failure::config(format!(
            "no daemon is listening on {}: {error}",
            socket.display()
        ))
    })?;
    let mut writer = &stream;
    writer
        .write_all(line.as_bytes())
        .and_then(|()| writer.write_all(b"\n"))
        .and_then(|()| writer.flush())
        .map_err(|error| Failure::runtime(format!("the socket took nothing: {error}")))?;

    let mut reader = BufReader::new(&stream);
    let mut answer = String::new();
    reader
        .read_line(&mut answer)
        .map_err(|error| Failure::runtime(format!("the socket said nothing: {error}")))?;
    match intent {
        Intent::Fork { .. } => {
            let (branch, depth) = exit::forked_reply(&answer)?;
            Ok(if invocation.json {
                report::fork_json(branch, depth)
            } else {
                report::fork_text(branch, depth)
            })
        },
        other => {
            exit::taken_reply(&answer)?;
            Ok(format!("{}: taken", other.name()))
        },
    }
}

/// What the log says the agent is doing, whether or not the daemon is up.
fn status(invocation: &Invocation) -> Result<String, Failure> {
    let path = store_path(invocation)?;
    if !path.exists() {
        return Err(Failure::config(format!(
            "no store at {}: has the daemon ever run?",
            path.display()
        )));
    }
    let found = agentctl::status::read(&path).map_err(|error| report::store_failure(&error))?;
    Ok(if invocation.json {
        report::status_json(&found)
    } else {
        report::status_text(&found)
    })
}
