//! Classification of OS-level sandbox denials into structured `CloudEvents`.
//!
//! A confined command refused by the OS exits non-zero and prints a denial
//! message; a spawned host binary has no other channel to report it. This
//! module recognises those messages, assigns a stable reason, extracts the
//! denied path where one is named, and builds a `sandbox.violation.*` event —
//! so the denial becomes durable, correlated state instead of an opaque exit
//! code.
//!
//! The classifier is heuristic by nature: it reads the process output. It is
//! deliberately conservative — an unrecognised failure is *not* reported as a
//! violation — so a normal command that fails for its own reasons is not
//! mislabelled as a sandbox denial.

use agentd_events::Event;
use serde_json::json;

use crate::events::{ACTION_EXEC, RESOURCE_SHELL};
use crate::executor::ExecResult;

/// The `CloudEvents` type of a filesystem denial.
pub const VIOLATION_FILESYSTEM: &str = "sandbox.violation.filesystem";
/// The `CloudEvents` type of a network denial.
pub const VIOLATION_NETWORK: &str = "sandbox.violation.network";

/// The longest output snippet carried on a violation event.
const MAX_SNIPPET: usize = 512;

/// Substrings that reliably mark an OS-enforced denial. Lower-case.
const DENIED_KEYWORDS: &[&str] = &[
    "operation not permitted",
    "permission denied",
    "read-only file system",
    "seccomp",
    "sandbox",
    "landlock",
    "failed to write file",
];

/// What the OS refused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ViolationKind {
    /// A filesystem operation was refused.
    FileSystem,
    /// A network operation was refused.
    Network,
}

impl ViolationKind {
    /// The stable string used in event data.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FileSystem => "filesystem",
            Self::Network => "network",
        }
    }
}

/// Why the OS refused, derived from the denial message.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ViolationReason {
    /// `operation not permitted`: a rule withheld the operation.
    OperationNotPermitted,
    /// `permission denied`: the kernel or the profile refused access.
    PermissionDenied,
    /// `read-only file system`: a write landed on a read-only path.
    ReadOnlyFileSystem,
    /// A policy mechanism named itself (`seccomp`, `landlock`, `sandbox`).
    PolicyDenied,
    /// `failed to write file`: the write itself failed under the profile.
    FailedToWriteFile,
    /// The process was killed by the platform's syscall-denial signal.
    SignalSyscall,
}

impl ViolationReason {
    /// The stable string used in event data.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OperationNotPermitted => "operation_not_permitted",
            Self::PermissionDenied => "permission_denied",
            Self::ReadOnlyFileSystem => "read_only_file_system",
            Self::PolicyDenied => "policy_denied",
            Self::FailedToWriteFile => "failed_to_write_file",
            Self::SignalSyscall => "signal_syscall",
        }
    }
}

/// A recognised OS denial.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Violation {
    /// What was refused.
    pub kind: ViolationKind,
    /// Why, as far as the message reveals.
    pub reason: ViolationReason,
    /// The denied path, when the message names one.
    pub path: Option<String>,
    /// A bounded snippet of the command output that carried the denial.
    pub output: String,
}

/// Classifies an execution result, returning a [`Violation`] for a recognised
/// OS denial and `None` otherwise.
///
/// A zero exit is never a violation. Otherwise the output must contain one of
/// the [`DENIED_KEYWORDS`]; a bare `command not found` (`127`) or a normal
/// application failure is not reported.
#[must_use]
pub fn classify_violation(result: &ExecResult) -> Option<Violation> {
    if result.exit_code == 0 {
        return None;
    }
    let combined = format!("{}\n{}", result.stdout, result.stderr);
    if is_signal_syscall(result.exit_code) {
        return Some(Violation {
            kind: ViolationKind::FileSystem,
            reason: ViolationReason::SignalSyscall,
            path: None,
            output: snippet(&combined),
        });
    }
    let lower = combined.to_lowercase();
    if !DENIED_KEYWORDS
        .iter()
        .any(|keyword| lower.contains(keyword))
    {
        return None;
    }
    Some(Violation {
        kind: kind_for(&lower),
        reason: reason_for(&lower),
        path: denied_path(&combined),
        output: snippet(&combined),
    })
}

/// Builds a `sandbox.violation.filesystem` or `sandbox.violation.network`
/// event for `violation`.
#[must_use]
pub fn violation_event(
    sandbox_id: &str,
    request_id: &str,
    agent_id: &str,
    command: &str,
    violation: &Violation,
) -> Event {
    let r#type = match violation.kind {
        ViolationKind::FileSystem => VIOLATION_FILESYSTEM,
        ViolationKind::Network => VIOLATION_NETWORK,
    };
    let mut data = json!({
        "sandbox_id": sandbox_id,
        "request_id": request_id,
        "agent_id": agent_id,
        "resource": RESOURCE_SHELL,
        "action": ACTION_EXEC,
        "command": command,
        "kind": violation.kind.as_str(),
        "reason": violation.reason.as_str(),
        "output": violation.output,
    });
    if let Some(path) = &violation.path {
        data["path"] = json!(path);
    }
    Event::new(r#type, data)
}

/// Classifies a refusal as network-related when the message names a connection.
///
/// The markers are connection-specific on purpose: matching the bare word
/// `network` would misclassify a filesystem denial whose path happens to
/// contain it.
fn kind_for(lower: &str) -> ViolationKind {
    const NETWORK_MARKERS: &[&str] = &[
        "connect",
        "socket",
        "network is unreachable",
        "network is down",
        "no route to host",
    ];
    if NETWORK_MARKERS.iter().any(|marker| lower.contains(marker)) {
        ViolationKind::Network
    } else {
        ViolationKind::FileSystem
    }
}

/// Maps the denial message to a stable reason, most specific first.
fn reason_for(lower: &str) -> ViolationReason {
    if lower.contains("read-only file system") {
        ViolationReason::ReadOnlyFileSystem
    } else if lower.contains("operation not permitted") {
        ViolationReason::OperationNotPermitted
    } else if lower.contains("permission denied") {
        ViolationReason::PermissionDenied
    } else if lower.contains("failed to write file") {
        ViolationReason::FailedToWriteFile
    } else {
        ViolationReason::PolicyDenied
    }
}

/// Extracts the denied path from a line of the form `prog: /path: reason`.
fn denied_path(output: &str) -> Option<String> {
    for line in output.lines() {
        let lower = line.to_lowercase();
        for suffix in [
            ": operation not permitted",
            ": permission denied",
            ": read-only file system",
        ] {
            let Some(position) = lower.find(suffix) else {
                continue;
            };
            let Some(prefix) = line.get(..position) else {
                continue;
            };
            if let Some(token) = prefix.split_whitespace().last() {
                return Some(token.to_string());
            }
        }
    }
    None
}

/// A bounded, trimmed output snippet.
fn snippet(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= MAX_SNIPPET {
        return trimmed.to_string();
    }
    let mut out: String = trimmed.chars().take(MAX_SNIPPET).collect();
    out.push('…');
    out
}

/// Whether the exit code is the platform's syscall-denial signal.
///
/// Only Linux uses `SIGSYS` for seccomp; macOS has no equivalent.
#[cfg(target_os = "linux")]
const fn is_signal_syscall(exit_code: i32) -> bool {
    exit_code == 128 + 31
}

/// Whether the exit code is the platform's syscall-denial signal.
#[cfg(not(target_os = "linux"))]
const fn is_signal_syscall(_exit_code: i32) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::{
        VIOLATION_FILESYSTEM, VIOLATION_NETWORK, ViolationKind, ViolationReason,
        classify_violation, violation_event,
    };
    use crate::executor::ExecResult;

    fn result(
        exit_code: i32,
        stdout: &str,
        stderr: &str,
    ) -> ExecResult {
        ExecResult {
            stdout: String::from(stdout),
            stderr: String::from(stderr),
            exit_code,
            denied: false,
        }
    }

    #[test]
    fn a_successful_command_is_never_a_violation() {
        assert_eq!(
            classify_violation(&result(0, "", "permission denied")),
            None
        );
    }

    #[test]
    fn an_unrelated_failure_is_not_a_violation() {
        assert_eq!(
            classify_violation(&result(127, "", "bash: foo: command not found")),
            None
        );
        assert_eq!(
            classify_violation(&result(1, "", "test failed: assertion")),
            None
        );
    }

    #[test]
    fn a_connection_denial_is_a_network_violation() {
        let violation =
            classify_violation(&result(7, "", "curl: (7) connect: Operation not permitted"))
                .expect("a denial");

        assert_eq!(violation.kind, ViolationKind::Network);
        assert_eq!(violation.reason, ViolationReason::OperationNotPermitted);
    }

    #[test]
    fn a_path_containing_network_is_still_a_filesystem_violation() {
        let violation = classify_violation(&result(
            1,
            "",
            "touch: /srv/network/out.txt: Operation not permitted",
        ))
        .expect("a denial");

        assert_eq!(violation.kind, ViolationKind::FileSystem);
    }

    #[test]
    fn a_read_denial_reports_permission_denied() {
        let violation = classify_violation(&result(1, "", "cat: /secret/token: Permission denied"))
            .expect("a denial");

        assert_eq!(violation.reason, ViolationReason::PermissionDenied);
        assert_eq!(violation.path.as_deref(), Some("/secret/token"));
    }

    #[test]
    fn a_read_only_write_reports_the_reason() {
        let violation =
            classify_violation(&result(1, "", "sh: /ro/out.txt: Read-only file system"))
                .expect("a denial");

        assert_eq!(violation.reason, ViolationReason::ReadOnlyFileSystem);
    }

    #[test]
    fn a_named_policy_mechanism_reports_policy_denied() {
        let violation =
            classify_violation(&result(1, "", "landlock: access denied")).expect("a denial");

        assert_eq!(violation.reason, ViolationReason::PolicyDenied);
    }

    #[test]
    fn a_long_output_is_truncated() {
        let stderr = format!("{}: Operation not permitted", "x".repeat(2_000));
        let violation = classify_violation(&result(1, "", &stderr)).expect("a denial");

        assert!(violation.output.chars().count() <= 513);
    }

    #[test]
    fn the_event_carries_the_contract_and_the_path() {
        let violation = classify_violation(&result(
            1,
            "",
            "touch: /etc/agentd: Operation not permitted",
        ))
        .expect("a denial");
        let event = violation_event("sbx-1", "req-1", "coder-1", "touch /etc/agentd", &violation);

        assert_eq!(event.r#type, VIOLATION_FILESYSTEM);
        assert_eq!(event.data["sandbox_id"], "sbx-1");
        assert_eq!(event.data["kind"], "filesystem");
        assert_eq!(event.data["reason"], "operation_not_permitted");
        assert_eq!(event.data["path"], "/etc/agentd");
        assert!(event.data["output"].as_str().is_some());
    }

    #[test]
    fn a_network_event_uses_the_network_type() {
        let violation = classify_violation(&result(7, "", "connect: Operation not permitted"))
            .expect("a denial");
        let event = violation_event("sbx-1", "req-1", "coder-1", "curl example.com", &violation);

        assert_eq!(event.r#type, VIOLATION_NETWORK);
        assert_eq!(event.data["kind"], "network");
    }
}
