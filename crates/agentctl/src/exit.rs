//! The exit table and the one error object every command reports.
//!
//! Both are pinned by [CLI commands](../../../docs/spec/interfaces/cli-commands.md),
//! so neither comes from `clap`. `clap` renders its own failures as text; this
//! module turns them into the same object a runtime failure produces.

use std::str::FromStr;

use agent_core::ids::BranchId;
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

    /// The exit a failure of this kind produces.
    #[must_use]
    pub const fn of_class(class: Class) -> Self {
        match class {
            Class::Usage => Self::Usage,
            Class::Runtime | Class::Unsupported => Self::Failed,
            Class::Policy => Self::Policy,
            Class::Storage => Self::Storage,
            Class::Corrupt => Self::Corrupt,
            Class::Locked => Self::Locked,
            Class::Config => Self::Config,
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

    /// The class a daemon's refusal names.
    ///
    /// The socket's vocabulary has three words for a runtime failure that the
    /// error object does not: a bad line, a command it does not know, and one
    /// that carried nothing. They all exit 64 and print as `runtime`.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "usage" => Some(Self::Usage),
            "runtime" | "failed" | "malformed" | "unknown" | "empty" => Some(Self::Runtime),
            "policy" => Some(Self::Policy),
            "storage" => Some(Self::Storage),
            "corrupt" => Some(Self::Corrupt),
            "locked" => Some(Self::Locked),
            "config" => Some(Self::Config),
            "unsupported" => Some(Self::Unsupported),
            _ => None,
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
    /// A failure at runtime, which is the daemon or the socket, not the arguments.
    pub const fn runtime(message: String) -> Self {
        Self {
            exit: Exit::Failed,
            class: Class::Runtime,
            message,
            hint: None,
        }
    }

    /// Policy refused it, which is the daemon saying no.
    #[must_use]
    pub const fn policy(message: String) -> Self {
        Self {
            exit: Exit::Policy,
            class: Class::Policy,
            message,
            hint: None,
        }
    }

    /// The daemon refused a command, naming the kind of refusal.
    #[must_use]
    pub const fn refused(
        class: Class,
        message: String,
    ) -> Self {
        Self {
            exit: Exit::of_class(class),
            class,
            message,
            hint: None,
        }
    }

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

/// Reads the daemon's answer to a command that had nothing else to report.
///
/// # Errors
///
/// Returns a [`Failure`] when the line is not the JSON this socket documents, or
/// when the daemon says `ok: false`; the refusal's class and message are kept.
pub fn taken_reply(line: &str) -> Result<(), Failure> {
    accepted(line).map(|_| ())
}

/// Reads the branch and depth the daemon's fork answer names.
///
/// # Errors
///
/// Returns a [`Failure`] when the line is not the JSON this socket documents,
/// when the daemon says `ok: false`, or when `ok` omits the branch or depth the
/// fork command needs to print an answer.
pub fn forked_reply(line: &str) -> Result<(BranchId, u64), Failure> {
    let value = accepted(line)?;
    let branch = value.get("branch").and_then(Value::as_str).ok_or_else(|| {
        Failure::runtime("the daemon accepted the fork but named no branch".to_string())
    })?;
    let branch = BranchId::from_str(branch).map_err(|_| {
        Failure::runtime(
            "the daemon accepted the fork but named a branch that is not an id".to_string(),
        )
    })?;
    let depth = value.get("depth").and_then(Value::as_u64).ok_or_else(|| {
        Failure::runtime("the daemon accepted the fork but named no depth".to_string())
    })?;
    Ok((branch, depth))
}

fn accepted(line: &str) -> Result<Value, Failure> {
    let value: Value = serde_json::from_str(line.trim_end())
        .map_err(|_| Failure::runtime("the socket answered with something else".to_string()))?;
    match value.get("ok").and_then(Value::as_bool) {
        Some(true) => Ok(value),
        Some(false) => Err(refusal(&value)),
        None => Err(Failure::runtime(
            "the socket answered with something else".to_string(),
        )),
    }
}

fn refusal(value: &Value) -> Failure {
    let message = value
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("the daemon refused it");
    let class = value
        .get("class")
        .and_then(Value::as_str)
        .and_then(Class::from_name)
        .unwrap_or(Class::Runtime);
    Failure::refused(class, message.to_string())
}

#[cfg(test)]
mod tests {
    use super::{Class, Exit, Failure, forked_reply, taken_reply};
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

    #[test]
    fn every_class_has_a_name_it_reads_back_from() {
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
            assert_eq!(Class::from_name(name), Some(class));
        }
    }

    #[test]
    fn the_sockets_runtime_words_all_read_as_runtime() {
        for name in ["failed", "malformed", "unknown", "empty"] {
            assert_eq!(Class::from_name(name), Some(Class::Runtime), "{name}");
        }
    }

    #[test]
    fn a_name_that_is_not_a_class_is_not_guessed() {
        assert_eq!(Class::from_name("weird"), None);
        assert_eq!(Class::from_name(""), None);
    }

    #[test]
    fn every_class_maps_to_its_exit_code() {
        let table = [
            (Class::Usage, 2),
            (Class::Runtime, 64),
            (Class::Policy, 65),
            (Class::Storage, 70),
            (Class::Corrupt, 74),
            (Class::Locked, 75),
            (Class::Config, 78),
            (Class::Unsupported, 64),
        ];
        for (class, code) in table {
            assert_eq!(Exit::of_class(class).code(), code, "{class:?}");
        }
    }

    #[test]
    fn a_refusal_keeps_the_class_and_message_it_was_given() {
        let failure = Failure::refused(Class::Policy, "the fork is inside a call".to_string());
        assert_eq!(failure.exit, Exit::Policy);
        assert_eq!(failure.class, Class::Policy);
        assert_eq!(failure.message, "the fork is inside a call");
        assert!(failure.hint.is_none());
    }

    #[test]
    fn a_taken_reply_is_ok_and_ignores_extra_fields() {
        assert_eq!(taken_reply(r#"{"ok":true}"#), Ok(()));
        assert_eq!(
            taken_reply(r#"{"ok":true,"branch":"b-0123456789abcdef"}"#),
            Ok(())
        );
    }

    #[test]
    fn every_refusal_class_from_the_socket_becomes_its_exit() {
        let table = [
            ("usage", 2, "usage"),
            ("policy", 65, "policy"),
            ("storage", 70, "storage"),
            ("corrupt", 74, "corrupt"),
            ("failed", 64, "runtime"),
            ("malformed", 64, "runtime"),
            ("unknown", 64, "runtime"),
            ("empty", 64, "runtime"),
        ];
        for (name, code, class) in table {
            let line = json!({"ok": false, "class": name, "message": "no"}).to_string();
            let failure = taken_reply(&line).expect_err("refused");
            assert_eq!(failure.exit.code(), code, "{name}");
            assert_eq!(failure.class.name(), class, "{name}");
            assert_eq!(failure.message, "no", "{name}");
        }
    }

    #[test]
    fn an_unknown_refusal_class_is_a_runtime_failure() {
        let failure =
            taken_reply(r#"{"ok":false,"class":"weird","message":"no"}"#).expect_err("refused");
        assert_eq!(failure.exit.code(), 64);
        assert_eq!(failure.class, Class::Runtime);
        assert_eq!(failure.message, "no");
    }

    #[test]
    fn a_refusal_without_a_class_or_message_still_reports_runtime() {
        let failure = taken_reply(r#"{"ok":false}"#).expect_err("refused");
        assert_eq!(failure.exit.code(), 64);
        assert_eq!(failure.class, Class::Runtime);
        assert_eq!(failure.message, "the daemon refused it");
    }

    #[test]
    fn a_line_that_is_not_an_ok_answer_is_a_runtime_failure() {
        for line in [
            r#"{"nope":true}"#,
            r#"{"ok":"yes"}"#,
            r#"{"ok":1}"#,
            r"{}",
            r"true",
            "not json",
        ] {
            let failure = taken_reply(line).expect_err("malformed");
            assert_eq!(failure.exit.code(), 64, "{line}");
            assert_eq!(failure.class, Class::Runtime, "{line}");
        }
    }

    #[test]
    fn a_fork_reply_names_the_new_branch_and_its_depth() {
        let (branch, depth) =
            forked_reply(r#"{"ok":true,"branch":"b-0123456789abcdef","depth":3}"#)
                .expect("a fork answer");
        assert_eq!(branch.to_string(), "b-0123456789abcdef");
        assert_eq!(depth, 3);
    }

    #[test]
    fn a_fork_reply_without_a_branch_or_depth_is_a_runtime_failure() {
        for line in [
            r#"{"ok":true}"#,
            r#"{"ok":true,"depth":3}"#,
            r#"{"ok":true,"branch":"b-0123456789abcdef"}"#,
            r#"{"ok":true,"branch":"nope","depth":3}"#,
            r#"{"ok":true,"branch":"b-0123456789abcdef","depth":-1}"#,
        ] {
            let failure = forked_reply(line).expect_err("unusable answer");
            assert_eq!(failure.exit.code(), 64, "{line}");
            assert_eq!(failure.class, Class::Runtime, "{line}");
        }
    }

    #[test]
    fn a_refused_fork_reply_reports_the_refusal_not_a_missing_branch() {
        let failure = forked_reply(
            r#"{"ok":false,"class":"policy","message":"the fork point leaves a call open"}"#,
        )
        .expect_err("refused");
        assert_eq!(failure.exit.code(), 65);
        assert_eq!(failure.class, Class::Policy);
        assert_eq!(failure.message, "the fork point leaves a call open");
    }
}
