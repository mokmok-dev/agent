//! The exit table and the one error object every command reports.
//!
//! Both are pinned by [CLI commands](../../../docs/spec/interfaces/cli-commands.md),
//! so neither comes from `clap`. `clap` renders its own failures as text; this
//! module turns them into the same object a runtime failure produces.

use serde_json::{Value, json};

/// What the process exits with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// The command did what the operator asked.
    Ok,
    /// The arguments were not usable.
    Usage,
    /// The command failed at runtime.
    Failed,
    /// Policy refused the command.
    Policy,
    /// Storage failed, including a full disk.
    Storage,
    /// The store is corrupt.
    Corrupt,
    /// Another process holds the store lock.
    Locked,
    /// The configuration is wrong.
    Config,
}

impl Exit {
    /// The number a shell reads.
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Self::Ok => 0,
            Self::Usage => 2,
            Self::Failed => 64,
            Self::Policy => 65,
            Self::Storage => 70,
            Self::Corrupt => 74,
            Self::Locked => 75,
            Self::Config => 78,
        }
    }
}

/// What kind of failure this is, in a word a script can match on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    Usage,
    Runtime,
    Policy,
    Storage,
    Corrupt,
    Locked,
    Config,
    Unsupported,
}

impl Class {
    /// The name the error object carries.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Usage => "usage",
            Self::Runtime => "runtime",
            Self::Policy => "policy",
            Self::Storage => "storage",
            Self::Corrupt => "corrupt",
            Self::Locked => "locked",
            Self::Config => "config",
            Self::Unsupported => "unsupported",
        }
    }
}

/// A failure, carrying everything the text and the JSON shape print.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub exit: Exit,
    pub class: Class,
    pub message: String,
    pub hint: Option<String>,
}

impl Failure {
    /// The arguments could not be read.
    #[must_use]
    pub fn usage(message: String) -> Self {
        Self {
            exit: Exit::Usage,
            class: Class::Usage,
            message,
            hint: Some(
                "run `agentctl --help` for the surface, or `agentctl <command> --help` for one command"
                    .to_string(),
            ),
        }
    }

    /// The command exists in the interface spec and has nothing behind it here.
    #[must_use]
    pub fn unsupported(
        command: &str,
        needs: &str,
    ) -> Self {
        Self {
            exit: Exit::Failed,
            class: Class::Unsupported,
            message: format!(
                "`agentctl {command}` is not available in this build: it needs {needs}"
            ),
            hint: Some("this build answers `version` and `verify` only".to_string()),
        }
    }

    /// The state directory could not be worked out.
    #[must_use]
    pub fn config(message: String) -> Self {
        Self {
            exit: Exit::Config,
            class: Class::Config,
            message,
            hint: Some("pass `--state-dir <path>`, or set `STATE_DIRECTORY`".to_string()),
        }
    }

    /// The object a script reads, with the field set the spec fixes.
    #[must_use]
    pub fn object(&self) -> Value {
        json!({
            "error": {
                "code": self.exit.code(),
                "class": self.class.name(),
                "message": self.message,
                "hint": self.hint.clone().map_or(Value::Null, Value::String),
            },
        })
    }

    /// What a person reads.
    #[must_use]
    pub fn text(&self) -> String {
        let message = &self.message;
        self.hint.as_ref().map_or_else(
            || format!("error: {message}"),
            |hint| format!("error: {message}\nhint: {hint}"),
        )
    }

    /// The one line to print, text or JSON.
    #[must_use]
    pub fn render(
        &self,
        json: bool,
    ) -> String {
        if json {
            self.object().to_string()
        } else {
            self.text()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Class, Exit, Failure};
    use serde_json::json;

    #[test]
    fn every_exit_code_is_the_one_the_spec_names() {
        let table = [
            (Exit::Ok, 0),
            (Exit::Usage, 2),
            (Exit::Failed, 64),
            (Exit::Policy, 65),
            (Exit::Storage, 70),
            (Exit::Corrupt, 74),
            (Exit::Locked, 75),
            (Exit::Config, 78),
        ];
        for (exit, code) in table {
            assert_eq!(exit.code(), code);
        }
    }

    #[test]
    fn every_class_has_its_own_name() {
        let table = [
            (Class::Usage, "usage"),
            (Class::Runtime, "runtime"),
            (Class::Policy, "policy"),
            (Class::Storage, "storage"),
            (Class::Corrupt, "corrupt"),
            (Class::Locked, "locked"),
            (Class::Config, "config"),
            (Class::Unsupported, "unsupported"),
        ];
        for (class, name) in table {
            assert_eq!(class.name(), name);
        }
    }

    #[test]
    fn a_usage_failure_points_at_help() {
        let failure = Failure::usage("unknown command".to_string());
        assert_eq!(failure.exit, Exit::Usage);
        assert_eq!(failure.class, Class::Usage);
        assert_eq!(failure.message, "unknown command");
        assert!(failure.hint.is_some_and(|hint| hint.contains("--help")));
    }

    #[test]
    fn an_unsupported_failure_names_the_command_and_what_it_needs() {
        let failure = Failure::unsupported("send", "the command socket at bus.sock");
        assert_eq!(failure.exit, Exit::Failed);
        assert_eq!(failure.class, Class::Unsupported);
        assert_eq!(
            failure.message,
            "`agentctl send` is not available in this build: it needs the command socket at bus.sock"
        );
        assert!(failure.hint.is_some_and(|hint| hint.contains("verify")));
    }

    #[test]
    fn a_config_failure_names_the_override() {
        let failure = Failure::config("no state directory".to_string());
        assert_eq!(failure.exit, Exit::Config);
        assert_eq!(failure.class, Class::Config);
        assert!(
            failure
                .hint
                .is_some_and(|hint| hint.contains("--state-dir"))
        );
    }

    #[test]
    fn the_error_object_holds_code_class_message_and_hint() {
        let failure = Failure::unsupported("gc", "blob storage");
        assert_eq!(
            failure.object(),
            json!({
                "error": {
                    "code": 64,
                    "class": "unsupported",
                    "message": "`agentctl gc` is not available in this build: it needs blob storage",
                    "hint": "this build answers `version` and `verify` only",
                },
            })
        );
    }

    #[test]
    fn a_missing_hint_is_null_rather_than_absent() {
        let failure = Failure {
            exit: Exit::Locked,
            class: Class::Locked,
            message: "the store is locked".to_string(),
            hint: None,
        };
        assert_eq!(
            failure.object(),
            json!({
                "error": {
                    "code": 75,
                    "class": "locked",
                    "message": "the store is locked",
                    "hint": null,
                },
            })
        );
    }

    #[test]
    fn text_carries_the_hint_when_there_is_one() {
        let failure = Failure::usage("bad".to_string());
        assert!(failure.text().starts_with("error: bad\nhint: "));
    }

    #[test]
    fn text_omits_the_hint_line_when_there_is_none() {
        let failure = Failure {
            exit: Exit::Failed,
            class: Class::Runtime,
            message: "the turn failed".to_string(),
            hint: None,
        };
        assert_eq!(failure.text(), "error: the turn failed");
    }

    #[test]
    fn render_picks_one_shape() {
        let failure = Failure::unsupported("status", "the status report");
        assert_eq!(failure.render(false), failure.text());
        assert_eq!(failure.render(true), failure.object().to_string());
        assert!(failure.render(true).starts_with("{\"error\":"));
    }
}
