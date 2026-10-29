//! The one program a model reaches a tool through.
//!
//! It reads one JSON object on standard input and writes one JSON object on
//! standard output. It exits zero for every outcome it handled, including a
//! request the tool refused, because the model has to read that answer. A
//! non-zero exit means this process failed.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::io::{Read as _, Write as _};
use std::process::ExitCode;

use agent_tools::{Allowlist, Ctx, Programs, Workspace, registry};

/// A mistake in the arguments, which is neither a tool answer nor a crash.
const USAGE: u8 = 2;
/// The tool could not read its input, its workspace, or its output.
const FAILED: u8 = 1;

fn main() -> ExitCode {
    let mut arguments = std::env::args_os().skip(1);
    let Some(command) = arguments.next() else {
        return usage();
    };
    let Ok(command) = command.into_string() else {
        return usage();
    };
    match command.as_str() {
        "--list" => list(),
        "--schema" => {
            let Some(name) = arguments.next() else {
                return usage();
            };
            let Ok(name) = name.into_string() else {
                return usage();
            };
            schema(&name)
        },
        name if name.starts_with('-') => usage(),
        name => dispatch(name, arguments.collect()),
    }
}

fn list() -> ExitCode {
    let mut text = String::new();
    for tool in registry() {
        let _ = writeln!(text, "{}\t{}", tool.name, tool.description);
    }
    emit(&text)
}

fn schema(name: &str) -> ExitCode {
    let Some(tool) = registry().iter().find(|tool| tool.name == name) else {
        return misuse(&format!("there is no tool named {name}"));
    };
    let text =
        serde_json::to_string_pretty(&(tool.parameters)()).unwrap_or_else(|_| String::from("{}"));
    emit(&text)
}

fn dispatch(
    name: &str,
    arguments: Vec<OsString>,
) -> ExitCode {
    let Some(tool) = registry().iter().find(|tool| tool.name == name) else {
        return misuse(&format!("there is no tool named {name}"));
    };
    let mut allowed = Vec::new();
    for argument in arguments {
        let Ok(argument) = argument.into_string() else {
            return usage();
        };
        match argument.strip_prefix("--allow=") {
            Some(program) if tool.programs == Programs::Listed => {
                allowed.push(program.to_string());
            },
            _ => return misuse(&format!("{} does not take {argument}", tool.name)),
        }
    }
    let allow = match Allowlist::of(&allowed) {
        Ok(allow) => allow,
        Err(error) => return misuse(&error.to_string()),
    };
    let directory = match std::env::current_dir() {
        Ok(directory) => directory,
        Err(error) => return fail(&format!("cannot read the working directory: {error}")),
    };
    let workspace = match Workspace::open(&directory) {
        Ok(workspace) => workspace,
        Err(error) => return fail(&format!("{}: {}", tool.name, error.message())),
    };
    let ctx = Ctx::capture(&workspace, &allow, tool.env);

    let mut input = Vec::new();
    if let Err(error) = std::io::stdin().read_to_end(&mut input) {
        return fail(&format!("cannot read standard input: {error}"));
    }
    emit_bytes(&(tool.call)(&input, &ctx))
}

fn emit(text: &str) -> ExitCode {
    write_out(text.as_bytes())
}

fn emit_bytes(bytes: &[u8]) -> ExitCode {
    let mut framed = bytes.to_vec();
    framed.push(b'\n');
    write_out(&framed)
}

fn write_out(bytes: &[u8]) -> ExitCode {
    let mut out = std::io::stdout().lock();
    if let Err(error) = out.write_all(bytes) {
        return fail(&format!("cannot write the answer: {error}"));
    }
    ExitCode::SUCCESS
}

fn usage() -> ExitCode {
    let _ = writeln!(
        std::io::stderr(),
        "usage: agent-tool --list | --schema <name> | <name> [--allow=<program>...]"
    );
    ExitCode::from(USAGE)
}

fn misuse(said: &str) -> ExitCode {
    let _ = writeln!(std::io::stderr(), "agent-tool: {said}");
    ExitCode::from(USAGE)
}

fn fail(said: &str) -> ExitCode {
    let _ = writeln!(std::io::stderr(), "agent-tool: {said}");
    ExitCode::from(FAILED)
}
