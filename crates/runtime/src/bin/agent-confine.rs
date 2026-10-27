//! The launcher: puts the bounds on this process and becomes the tool.
//!
//! It is a separate binary because a bound has to be installed by the process that
//! will hold it. Doing that in the daemon would bound the daemon, and doing it
//! between fork and exec would mean the daemon's own code deciding when it is safe
//! to allocate, which is a question this program does not have to answer: it is
//! still one ordinary process here, with nothing else on its thread.
//!
//! Nothing in it reads the configuration or the log. It knows the bounds its own
//! command line names, and the program to become.

use std::ffi::OsString;
use std::os::unix::process::CommandExt as _;
use std::process::ExitCode;

use agent_runtime::confine::{CHECK, bounds_from, confine};

/// What the launcher exits with when the kernel refused a bound.
///
/// The daemon reads this as "no tool ran and nothing happened", which is the
/// truth: the program was never started.
const REFUSED: u8 = 78;

fn main() -> ExitCode {
    let mut arguments: Vec<OsString> = std::env::args_os().skip(1).collect();
    let check = arguments.first().is_some_and(|first| first == CHECK);
    if check {
        arguments.remove(0);
    }

    let (bounds, program) = match bounds_from(&arguments) {
        Ok(read) => read,
        Err(said) => {
            eprintln!("agent-confine: {said}");
            return ExitCode::from(REFUSED);
        },
    };
    if let Err(said) = confine(&bounds) {
        eprintln!("agent-confine: {said}");
        return ExitCode::from(REFUSED);
    }
    if check {
        return ExitCode::SUCCESS;
    }

    let Some((name, rest)) = program.split_first() else {
        eprintln!("agent-confine: no program to become");
        return ExitCode::from(REFUSED);
    };
    let error = std::process::Command::new(name).args(rest).exec();
    eprintln!(
        "agent-confine: cannot become {}: {error}",
        name.to_string_lossy()
    );
    ExitCode::from(REFUSED)
}
