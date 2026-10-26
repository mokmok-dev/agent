//! The command line the daemon accepts.

use std::path::PathBuf;

/// What the operator asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Args {
    /// A configured daemon: a model, a provider, and where the store lives.
    pub config: Option<PathBuf>,
    /// A store and nothing else, which can write turns but not answer them.
    pub state_dir: Option<PathBuf>,
    pub once: bool,
    /// Where the command socket listens. Defaults to the state directory.
    pub bus: Option<PathBuf>,
}

/// How to run the daemon.
pub const USAGE: &str =
    "usage: agentd (--config <path> | --state-dir <path>) [--bus <path>] [--once]";

/// Why the arguments were not usable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArgError {
    /// The operator asked for help.
    Help,
    /// A flag was given without its value.
    MissingValue(&'static str),
    /// A flag the daemon does not know was given.
    Unknown(String),
    /// A required flag was not given.
    Missing(&'static str),
    /// Two ways of naming the state were given at once.
    Conflict,
}

impl std::fmt::Display for ArgError {
    fn fmt(
        &self,
        formatter: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        match self {
            Self::Help => write!(formatter, "{USAGE}"),
            Self::MissingValue(flag) => write!(formatter, "{flag} needs a value"),
            Self::Unknown(flag) => write!(formatter, "unknown argument: {flag}"),
            Self::Missing(flag) => write!(formatter, "{flag} is required"),
            Self::Conflict => write!(formatter, "--config and --state-dir cannot both be given"),
        }
    }
}

impl std::error::Error for ArgError {}

/// Parses the arguments that follow the program name.
///
/// # Errors
///
/// Returns [`ArgError`] when a flag is unknown, a value is missing, or the
/// state directory was not given.
pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<Args, ArgError> {
    let mut state_dir = None;
    let mut config = None;
    let mut once = false;
    let mut bus = None;
    let mut rest = args.into_iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--state-dir" => {
                let value = rest.next().ok_or(ArgError::MissingValue("--state-dir"))?;
                state_dir = Some(PathBuf::from(value));
            },
            "--config" => {
                let value = rest.next().ok_or(ArgError::MissingValue("--config"))?;
                config = Some(PathBuf::from(value));
            },
            "--once" => once = true,
            "--bus" => {
                let value = rest.next().ok_or(ArgError::MissingValue("--bus"))?;
                bus = Some(PathBuf::from(value));
            },
            "--help" | "-h" => return Err(ArgError::Help),
            other => return Err(ArgError::Unknown(other.to_string())),
        }
    }
    if config.is_some() && state_dir.is_some() {
        return Err(ArgError::Conflict);
    }
    if config.is_none() && state_dir.is_none() {
        return Err(ArgError::Missing("--config"));
    }
    Ok(Args {
        config,
        state_dir,
        once,
        bus,
    })
}

#[cfg(test)]
mod tests {
    use super::{ArgError, Args, parse};
    use std::path::PathBuf;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn a_state_dir_is_enough() {
        assert_eq!(
            parse(args(&["--state-dir", "/var/lib/agent"])),
            Ok(Args {
                bus: None,
                config: None,
                state_dir: Some(PathBuf::from("/var/lib/agent")),
                once: false,
            })
        );
    }

    #[test]
    fn a_config_file_is_enough() {
        assert_eq!(
            parse(args(&["--config", "/etc/agent.toml"])),
            Ok(Args {
                bus: None,
                config: Some(PathBuf::from("/etc/agent.toml")),
                state_dir: None,
                once: false,
            })
        );
    }

    #[test]
    fn both_ways_of_naming_the_state_at_once_is_refused() {
        assert_eq!(
            parse(args(&[
                "--config",
                "/etc/agent.toml",
                "--state-dir",
                "/tmp"
            ])),
            Err(ArgError::Conflict)
        );
    }

    #[test]
    fn once_is_recognised() {
        let parsed = parse(args(&["--once", "--state-dir", "/tmp"])).expect("parses");
        assert!(parsed.once);
    }

    #[test]
    fn order_does_not_matter() {
        let first = parse(args(&["--once", "--state-dir", "/tmp"])).expect("parses");
        let second = parse(args(&["--state-dir", "/tmp", "--once"])).expect("parses");
        assert_eq!(first, second);
    }

    #[test]
    fn naming_no_state_at_all_is_refused() {
        assert_eq!(parse(args(&[])), Err(ArgError::Missing("--config")));
    }

    #[test]
    fn a_flag_without_a_value_is_refused() {
        assert_eq!(
            parse(args(&["--state-dir"])),
            Err(ArgError::MissingValue("--state-dir"))
        );
    }

    #[test]
    fn an_unknown_flag_is_refused() {
        assert_eq!(
            parse(args(&["--verbose"])),
            Err(ArgError::Unknown("--verbose".to_string()))
        );
    }

    #[test]
    fn help_is_its_own_outcome() {
        assert_eq!(parse(args(&["--help"])), Err(ArgError::Help));
        assert_eq!(parse(args(&["-h"])), Err(ArgError::Help));
    }

    #[test]
    fn every_error_explains_itself() {
        for error in [
            ArgError::Help,
            ArgError::MissingValue("--state-dir"),
            ArgError::Unknown("--x".to_string()),
            ArgError::Missing("--state-dir"),
        ] {
            assert_ne!(error.to_string(), "");
        }
    }
}
