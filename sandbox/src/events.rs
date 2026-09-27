//! The `CloudEvents` a confined command produces on the event bus.
//!
//! Milestone 3 turns a terminal [`ExecOutcome`] into the events
//! `docs/sandbox/events.md` specifies: always [`EXEC_COMPLETED`], and a
//! [`VIOLATION_FILESYSTEM`] or [`VIOLATION_NETWORK`] for each distinct
//! OS-enforced denial the command hit.
//!
//! # The boundary with the bus
//!
//! The sandbox authors only the attributes a producer owns: `type`, `subject`,
//! `data`, and the `traceparent` extension. The bus assigns `specversion`,
//! `source`, `id`, `time`, and `sequence` at commit. [`Event`] is therefore the
//! producer half of an `Incoming` envelope, and the sandbox does not depend on
//! the bus crate: the daemon that owns both joins them.
//!
//! # The classifier is a heuristic
//!
//! A denial is recognised from the command's `stderr`, because that is where the
//! kernel's `errno` message lands. A command that prints the same text without a
//! kernel denial is indistinguishable from one that was denied; the classifier
//! is deliberately conservative and matches only a small set of exact libc
//! messages, so an unrelated non-zero exit produces no violation.

use std::path::Path;

use serde_json::{Map, Value, json};

use crate::egress::RequestId;
use crate::executor::{ExecOutcome, ProcessOutcome};
use crate::policy::HostPort;

/// The terminal state of a one-shot execution.
pub const EXEC_COMPLETED: &str = "agent.sandbox.exec.completed";
/// A long-lived confined process was spawned.
pub const PROCESS_STARTED: &str = "agent.sandbox.process.started";
/// The terminal state of a long-lived process: exit code and duration.
pub const PROCESS_EXITED: &str = "agent.sandbox.process.exited";
/// The kernel refused a filesystem operation.
pub const VIOLATION_FILESYSTEM: &str = "agent.sandbox.violation.filesystem";
/// The kernel refused a network operation.
pub const VIOLATION_NETWORK: &str = "agent.sandbox.violation.network";
/// An authority added a destination to the mutable egress allowlist.
pub const EGRESS_RULE_ADDED: &str = "agent.sandbox.egress.rule_added";
/// An authority removed a destination from the mutable egress allowlist.
pub const EGRESS_RULE_REVOKED: &str = "agent.sandbox.egress.rule_revoked";
/// A connection arrived for an unlisted destination and an approver is configured.
pub const EGRESS_REQUESTED: &str = "agent.sandbox.egress.requested";
/// An approver allowed the destination; the proxy opens the tunnel.
pub const EGRESS_GRANTED: &str = "agent.sandbox.egress.granted";
/// An approver refused the destination; the proxy answers `403`.
pub const EGRESS_DENIED: &str = "agent.sandbox.egress.denied";
/// The deadline passed with no decision, or an approver withdrew it; the proxy
/// answers `403`, but the recorded reason is a cancellation.
pub const EGRESS_CANCELLED: &str = "agent.sandbox.egress.cancelled";

/// How much of the command's `stderr` a violation event carries.
const OUTPUT_SNIPPET_BYTES: usize = 4096;

/// What an event is about: the sandbox, the command, and the trace it continues.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxContext {
    /// The `sandbox_id`, which becomes the event `subject`.
    pub sandbox_id: String,
    /// The command, which becomes `data.command`.
    pub command: String,
    /// The W3C `traceparent` the event carries, if any.
    pub traceparent: Option<String>,
}

impl SandboxContext {
    /// A context for `command` in sandbox `sandbox_id`, with no trace.
    pub fn new(
        sandbox_id: impl Into<String>,
        command: impl Into<String>,
    ) -> Self {
        Self {
            sandbox_id: sandbox_id.into(),
            command: command.into(),
            traceparent: None,
        }
    }

    /// Continue `traceparent`, returning the updated context.
    #[must_use]
    pub fn traced(
        mut self,
        traceparent: impl Into<String>,
    ) -> Self {
        self.traceparent = Some(traceparent.into());
        self
    }
}

/// The producer half of a `CloudEvent`: the attributes the sandbox authors.
///
/// The bus adds `specversion`, `source`, `id`, `time`, and `sequence` when the
/// daemon commits this as an `Incoming` envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    /// The event type, one of the module constants.
    pub ty: &'static str,
    /// The `sandbox_id`, which the bus carries as `subject`.
    pub subject: String,
    /// The payload, which the bus carries as `data`.
    pub data: Value,
    /// The W3C `traceparent` extension, if any.
    pub traceparent: Option<String>,
}

impl Event {
    /// The `exec.completed` event for `outcome`.
    #[must_use]
    pub fn completed(
        outcome: &ExecOutcome,
        context: &SandboxContext,
    ) -> Self {
        Self {
            ty: EXEC_COMPLETED,
            subject: context.sandbox_id.clone(),
            data: json!({
                "sandbox_id": context.sandbox_id,
                "command": context.command,
                "exit_code": outcome.code,
                "duration_ms": u64::try_from(outcome.duration.as_millis()).unwrap_or(u64::MAX),
                "stdout_bytes": outcome.stdout.len(),
                "stderr_bytes": outcome.stderr.len(),
                "timed_out": outcome.timed_out,
            }),
            traceparent: context.traceparent.clone(),
        }
    }

    /// The `process.started` event for a long-lived process `pid`.
    ///
    /// A long-lived process is confined by one policy and its children inherit
    /// it, so there are no per-command permission events for it; its lifecycle
    /// events are the audit trail. The `pid` is the daemon-side process id, for
    /// correlating the started and exited events.
    #[must_use]
    pub fn process_started(
        context: &SandboxContext,
        pid: u32,
    ) -> Self {
        Self {
            ty: PROCESS_STARTED,
            subject: context.sandbox_id.clone(),
            data: json!({
                "sandbox_id": context.sandbox_id,
                "command": context.command,
                "pid": pid,
            }),
            traceparent: context.traceparent.clone(),
        }
    }

    /// The `process.exited` event for a long-lived process.
    #[must_use]
    pub fn process_exited(
        outcome: &ProcessOutcome,
        context: &SandboxContext,
    ) -> Self {
        Self {
            ty: PROCESS_EXITED,
            subject: context.sandbox_id.clone(),
            data: json!({
                "sandbox_id": context.sandbox_id,
                "command": context.command,
                "exit_code": outcome.code,
                "duration_ms": u64::try_from(outcome.duration.as_millis()).unwrap_or(u64::MAX),
                "killed": outcome.killed,
            }),
            traceparent: context.traceparent.clone(),
        }
    }

    /// One violation event per distinct OS-enforced denial in `outcome`.
    ///
    /// Repeats of the same `(kind, reason, denied_path)` collapse, so a command
    /// that writes a thousand denied files yields one event rather than a
    /// thousand. An outcome with no recognised denial yields none.
    #[must_use]
    pub fn violations(
        outcome: &ExecOutcome,
        context: &SandboxContext,
    ) -> Vec<Self> {
        let snippet = snippet(&outcome.stderr);
        classify(&outcome.stderr)
            .into_iter()
            .map(|violation| {
                let ty = match violation.kind {
                    ViolationKind::Filesystem => VIOLATION_FILESYSTEM,
                    ViolationKind::Network => VIOLATION_NETWORK,
                };
                let mut data = Map::new();
                data.insert("sandbox_id".to_owned(), json!(context.sandbox_id));
                data.insert("command".to_owned(), json!(context.command));
                data.insert("reason".to_owned(), json!(violation.reason.as_str()));
                if let Some(path) = &violation.denied_path {
                    data.insert("denied_path".to_owned(), json!(path));
                }
                data.insert("output".to_owned(), json!(snippet));
                Self {
                    ty,
                    subject: context.sandbox_id.clone(),
                    data: Value::Object(data),
                    traceparent: context.traceparent.clone(),
                }
            })
            .collect()
    }

    /// The `egress.rule_added` event for `destination` in `sandbox_id`.
    ///
    /// An authority publishes this to widen the mutable allowlist; the proxy
    /// applies it so subsequent connections to `host:port` are allowed. The
    /// payload is `host`, `port`, and the `sandbox_id` whose proxy owns the set,
    /// per `docs/sandbox/events.md`.
    #[must_use]
    pub fn egress_rule_added(
        sandbox_id: impl Into<String>,
        destination: &HostPort,
    ) -> Self {
        Self::rule(EGRESS_RULE_ADDED, sandbox_id, destination)
    }

    /// The `egress.rule_revoked` event for `destination` in `sandbox_id`.
    ///
    /// An authority publishes this to narrow the mutable allowlist; the proxy
    /// applies it so subsequent connections are no longer allowed by the rule,
    /// and closes any established tunnel the rule granted.
    #[must_use]
    pub fn egress_rule_revoked(
        sandbox_id: impl Into<String>,
        destination: &HostPort,
    ) -> Self {
        Self::rule(EGRESS_RULE_REVOKED, sandbox_id, destination)
    }

    /// The `egress.requested` event for `destination`, carrying `request_id`.
    ///
    /// The proxy publishes this as the trusted daemon when an unlisted
    /// destination has an approver. The id correlates the request with the one
    /// decision that answers it.
    #[must_use]
    pub fn egress_requested(
        sandbox_id: impl Into<String>,
        request_id: &RequestId,
        destination: &HostPort,
        traceparent: Option<String>,
    ) -> Self {
        Self::decision(
            EGRESS_REQUESTED,
            sandbox_id,
            request_id,
            destination,
            traceparent,
        )
    }

    /// The `egress.granted` event: an approver allowed the destination.
    #[must_use]
    pub fn egress_granted(
        sandbox_id: impl Into<String>,
        request_id: &RequestId,
        destination: &HostPort,
        traceparent: Option<String>,
    ) -> Self {
        Self::decision(
            EGRESS_GRANTED,
            sandbox_id,
            request_id,
            destination,
            traceparent,
        )
    }

    /// The `egress.denied` event: an approver refused the destination.
    #[must_use]
    pub fn egress_denied(
        sandbox_id: impl Into<String>,
        request_id: &RequestId,
        destination: &HostPort,
        traceparent: Option<String>,
    ) -> Self {
        Self::decision(
            EGRESS_DENIED,
            sandbox_id,
            request_id,
            destination,
            traceparent,
        )
    }

    /// The `egress.cancelled` event: the deadline passed, or an approver withdrew
    /// the request. Never a fabrication of a denial.
    #[must_use]
    pub fn egress_cancelled(
        sandbox_id: impl Into<String>,
        request_id: &RequestId,
        destination: &HostPort,
        traceparent: Option<String>,
    ) -> Self {
        Self::decision(
            EGRESS_CANCELLED,
            sandbox_id,
            request_id,
            destination,
            traceparent,
        )
    }

    /// A destination-decision event: `request_id`, `host`, `port`, and the
    /// optional `traceparent`, per `docs/sandbox/events.md`.
    fn decision(
        ty: &'static str,
        sandbox_id: impl Into<String>,
        request_id: &RequestId,
        destination: &HostPort,
        traceparent: Option<String>,
    ) -> Self {
        let sandbox_id = sandbox_id.into();
        Self {
            ty,
            subject: sandbox_id.clone(),
            data: json!({
                "sandbox_id": sandbox_id,
                "request_id": request_id.as_str(),
                "host": &destination.host,
                "port": destination.port,
            }),
            traceparent,
        }
    }

    /// A `rule_added` / `rule_revoked` event, whose payloads are identical.
    fn rule(
        ty: &'static str,
        sandbox_id: impl Into<String>,
        destination: &HostPort,
    ) -> Self {
        let sandbox_id = sandbox_id.into();
        Self {
            ty,
            subject: sandbox_id.clone(),
            data: json!({
                "sandbox_id": sandbox_id,
                "host": &destination.host,
                "port": destination.port,
            }),
            traceparent: None,
        }
    }
}

/// The kind of resource an OS-enforced denial concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViolationKind {
    /// The kernel refused a filesystem operation.
    Filesystem,
    /// The kernel refused a network operation.
    Network,
}

/// Why the kernel refused an operation.
///
/// Names follow `docs/sandbox/events.md`; each is the `errno` the kernel
/// returned, recovered from the message the command printed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// `EROFS`: writing outside a `write` entry, whose mount is read-only.
    ReadOnlyFileSystem,
    /// `EACCES`: reading or writing a path a `deny` entry masked.
    PermissionDenied,
    /// `EPERM`: the operation is not permitted, for example on a protected mount.
    OperationNotPermitted,
    /// `ENETUNREACH`: any IP egress, because the network namespace has no route.
    NetworkUnreachable,
    /// `EHOSTUNREACH`: the route exists but the host is unreachable.
    HostUnreachable,
}

impl Reason {
    /// Every reason, for the classifier to search.
    const ALL: [Self; 5] = [
        Self::ReadOnlyFileSystem,
        Self::PermissionDenied,
        Self::OperationNotPermitted,
        Self::NetworkUnreachable,
        Self::HostUnreachable,
    ];

    /// The `errno` message the kernel's denial prints, matched exactly.
    const fn message(self) -> &'static str {
        match self {
            Self::ReadOnlyFileSystem => "Read-only file system",
            Self::PermissionDenied => "Permission denied",
            Self::OperationNotPermitted => "Operation not permitted",
            Self::NetworkUnreachable => "Network is unreachable",
            Self::HostUnreachable => "No route to host",
        }
    }

    /// The resource kind the reason belongs to.
    const fn kind(self) -> ViolationKind {
        match self {
            Self::ReadOnlyFileSystem | Self::PermissionDenied | Self::OperationNotPermitted => {
                ViolationKind::Filesystem
            },
            Self::NetworkUnreachable | Self::HostUnreachable => ViolationKind::Network,
        }
    }

    /// The name recorded in the event, per `docs/sandbox/events.md`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnlyFileSystem => "ReadOnlyFileSystem",
            Self::PermissionDenied => "PermissionDenied",
            Self::OperationNotPermitted => "OperationNotPermitted",
            Self::NetworkUnreachable => "NetworkUnreachable",
            Self::HostUnreachable => "HostUnreachable",
        }
    }
}

/// One recognised OS-enforced denial.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    /// Whether the denial concerned the filesystem or the network.
    pub kind: ViolationKind,
    /// Why the kernel refused.
    pub reason: Reason,
    /// The path involved, when the message named an absolute one.
    pub denied_path: Option<String>,
}

/// Classify every distinct OS-enforced denial in `stderr`.
///
/// A line is a denial when it ends with a known `errno` message. Returns one
/// [`Violation`] per distinct `(kind, reason, denied_path)`, in first-seen order;
/// an unrelated non-zero exit yields none.
#[must_use]
pub fn classify(stderr: &[u8]) -> Vec<Violation> {
    let text = String::from_utf8_lossy(stderr);
    let mut found: Vec<Violation> = Vec::new();
    for line in text.lines() {
        if let Some(violation) = classify_line(line.trim_end())
            && !found.contains(&violation)
        {
            found.push(violation);
        }
    }
    found
}

/// Classify one line, or `None` when it is not a recognised denial.
fn classify_line(line: &str) -> Option<Violation> {
    let reason = Reason::ALL
        .into_iter()
        .find(|reason| line.ends_with(reason.message()))?;
    let denied_path = match reason.kind() {
        ViolationKind::Filesystem => denied_path(line, reason.message()),
        // A network denial names a host, not a filesystem path; the destination
        // it could not reach is not recoverable from the message.
        ViolationKind::Network => None,
    };
    Some(Violation {
        kind: reason.kind(),
        reason,
        denied_path,
    })
}

/// The absolute path a denial line names, if any.
///
/// The message is stripped first. The path is then the absolute token in the
/// last `: `-separated segment of what remains: a shell writes
/// `<shell>: line N: <path>: Read-only file system` (bash) or
/// `<shell>: N: cannot create <path>: Permission denied` (dash), and coreutils
/// writes `<program>: <path>: Permission denied`. In every case the last segment
/// is a short prefix — nothing, `line N`, or `cannot create ` — followed by the
/// path, so the path is that segment from its first `/`.
///
/// This is why the shell is not hard-coded: the daemon may run on a host whose
/// `/bin/sh` is bash, dash, or busybox, and the classifier must recover the path
/// from all of them.
fn denied_path(
    line: &str,
    message: &str,
) -> Option<String> {
    let head = line.strip_suffix(message)?.trim_end();
    let head = head.strip_suffix(':')?.trim_end();
    let segment = head.rsplit(": ").next().unwrap_or(head);
    let start = segment.find('/')?;
    let candidate = &segment[start..];
    Path::new(candidate)
        .is_absolute()
        .then(|| candidate.to_owned())
}

/// The last [`OUTPUT_SNIPPET_BYTES`] of `stderr`, lossily decoded.
fn snippet(stderr: &[u8]) -> String {
    let start = stderr.len().saturating_sub(OUTPUT_SNIPPET_BYTES);
    String::from_utf8_lossy(&stderr[start..]).into_owned()
}

#[cfg(test)]
mod tests {
    // Tests for the classifier and the event payloads. The classifier is driven
    // by the exact messages the kernel prints; the integration test in
    // `tests/events.rs` drives it with a real denied command.

    use std::time::Duration;

    use super::*;
    use crate::executor::ProcessOutcome;

    fn context() -> SandboxContext {
        SandboxContext::new("sbx-7", "cat /work/secret")
    }

    #[test]
    fn a_read_only_write_is_classified_with_its_path() {
        let stderr = b"sh: line 1: /work/out: Read-only file system\n";
        let found = classify(stderr);
        assert_eq!(
            found,
            vec![Violation {
                kind: ViolationKind::Filesystem,
                reason: Reason::ReadOnlyFileSystem,
                denied_path: Some("/work/out".to_owned()),
            }]
        );
    }

    #[test]
    fn a_denied_read_is_classified_with_its_path() {
        let stderr = b"cat: /work/secret.txt: Permission denied\n";
        assert_eq!(
            classify(stderr),
            vec![Violation {
                kind: ViolationKind::Filesystem,
                reason: Reason::PermissionDenied,
                denied_path: Some("/work/secret.txt".to_owned()),
            }]
        );
    }

    #[test]
    fn a_network_denial_is_classified_without_a_path() {
        let stderr = b"sh: connect: Network is unreachable\n";
        assert_eq!(
            classify(stderr),
            vec![Violation {
                kind: ViolationKind::Network,
                reason: Reason::NetworkUnreachable,
                denied_path: None,
            }]
        );
    }

    #[test]
    fn an_operation_not_permitted_is_filesystem() {
        let stderr = b"mknod: /dev/x: Operation not permitted\n";
        let found = classify(stderr);
        assert_eq!(
            found.first().map(|v| v.reason),
            Some(Reason::OperationNotPermitted)
        );
        assert_eq!(
            found.first().map(|v| v.kind),
            Some(ViolationKind::Filesystem)
        );
    }

    #[test]
    fn an_unrelated_non_zero_output_yields_no_violation() {
        // A missing file and a generic error are not sandbox violations.
        for stderr in [
            &b"cat: /work/missing: No such file or directory\n"[..],
            b"grep: unrecognized option\n",
            b"",
            b"some command failed\n",
        ] {
            assert!(classify(stderr).is_empty(), "{stderr:?} is not a violation");
        }
    }

    #[test]
    fn a_dash_read_only_write_is_classified_with_its_path() {
        // dash writes `<shell>: N: cannot create <path>: Read-only file system`,
        // with no `line`.
        let stderr = b"sh: 1: cannot create /work/out: Read-only file system\n";
        assert_eq!(
            classify(stderr),
            vec![Violation {
                kind: ViolationKind::Filesystem,
                reason: Reason::ReadOnlyFileSystem,
                denied_path: Some("/work/out".to_owned()),
            }]
        );
    }

    #[test]
    fn a_dash_permission_denied_is_classified_with_its_path() {
        let stderr = b"sh: 1: cannot create /work/out: Permission denied\n";
        assert_eq!(
            classify(stderr)
                .first()
                .and_then(|v| v.denied_path.as_deref()),
            Some("/work/out")
        );
    }

    #[test]
    fn a_message_without_a_path_has_no_denied_path() {
        assert_eq!(
            classify(b"Permission denied\n"),
            vec![Violation {
                kind: ViolationKind::Filesystem,
                reason: Reason::PermissionDenied,
                denied_path: None,
            }]
        );
    }

    #[test]
    fn distinct_denials_are_kept_and_repeats_collapse() {
        let stderr = b"cat: /a: Permission denied\ncat: /a: Permission denied\nsh: line 1: /b: Read-only file system\n";
        let found = classify(stderr);
        assert_eq!(found.len(), 2, "one per distinct denial");
        assert_eq!(found[0].denied_path.as_deref(), Some("/a"));
        assert_eq!(found[1].denied_path.as_deref(), Some("/b"));
    }

    #[test]
    fn the_exec_completed_payload_has_the_terminal_state() {
        let outcome = ExecOutcome {
            code: Some(1),
            duration: Duration::from_millis(12),
            stdout: b"out".to_vec(),
            stderr: b"err!".to_vec(),
            timed_out: false,
        };
        let event = Event::completed(&outcome, &context());
        assert_eq!(event.ty, EXEC_COMPLETED);
        assert_eq!(event.subject, "sbx-7");
        assert_eq!(event.data["exit_code"], json!(1));
        assert_eq!(event.data["duration_ms"], json!(12));
        assert_eq!(event.data["stdout_bytes"], json!(3));
        assert_eq!(event.data["stderr_bytes"], json!(4));
        assert_eq!(event.data["timed_out"], json!(false));
        assert_eq!(event.data["command"], json!("cat /work/secret"));
    }

    #[test]
    fn a_timed_out_outcome_records_it_and_a_signal_exit_has_no_code() {
        let outcome = ExecOutcome {
            code: None,
            duration: Duration::from_millis(300),
            stdout: Vec::new(),
            stderr: Vec::new(),
            timed_out: true,
        };
        let event = Event::completed(&outcome, &context());
        assert_eq!(event.data["exit_code"], Value::Null);
        assert_eq!(event.data["timed_out"], json!(true));
    }

    #[test]
    fn a_violation_event_names_the_reason_path_and_output() {
        let outcome = ExecOutcome {
            code: Some(1),
            duration: Duration::from_millis(1),
            stdout: Vec::new(),
            stderr: b"sh: line 1: /work/out: Read-only file system\n".to_vec(),
            timed_out: false,
        };
        let events = Event::violations(&outcome, &context());
        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(event.ty, VIOLATION_FILESYSTEM);
        assert_eq!(event.subject, "sbx-7");
        assert_eq!(event.data["reason"], json!("ReadOnlyFileSystem"));
        assert_eq!(event.data["denied_path"], json!("/work/out"));
        assert!(
            event.data["output"]
                .as_str()
                .is_some_and(|s| s.contains("Read-only"))
        );
    }

    #[test]
    fn a_network_violation_omits_the_denied_path() {
        let outcome = ExecOutcome {
            code: Some(1),
            duration: Duration::from_millis(1),
            stdout: Vec::new(),
            stderr: b"sh: connect: Network is unreachable\n".to_vec(),
            timed_out: false,
        };
        let events = Event::violations(&outcome, &context());
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].ty, VIOLATION_NETWORK);
        assert_eq!(events[0].data["reason"], json!("NetworkUnreachable"));
        assert!(
            events[0].data.get("denied_path").is_none(),
            "a network denial names no path"
        );
    }

    #[test]
    fn the_traceparent_is_carried_to_every_event() {
        let context = context().traced("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01");
        let outcome = ExecOutcome {
            code: Some(0),
            duration: Duration::from_millis(1),
            stdout: Vec::new(),
            stderr: b"sh: x: Permission denied\n".to_vec(),
            timed_out: false,
        };
        assert_eq!(
            Event::completed(&outcome, &context).traceparent.as_deref(),
            context.traceparent.as_deref()
        );
        assert_eq!(
            Event::violations(&outcome, &context)[0]
                .traceparent
                .as_deref(),
            context.traceparent.as_deref()
        );
    }

    #[test]
    fn a_process_started_event_names_the_command_and_pid() {
        let event = Event::process_started(&context(), 4242);
        assert_eq!(event.ty, PROCESS_STARTED);
        assert_eq!(event.subject, "sbx-7");
        assert_eq!(event.data["sandbox_id"], json!("sbx-7"));
        assert_eq!(event.data["command"], json!("cat /work/secret"));
        assert_eq!(event.data["pid"], json!(4242));
    }

    #[test]
    fn a_process_exited_event_carries_the_terminal_state() {
        let outcome = ProcessOutcome {
            code: Some(0),
            duration: Duration::from_millis(250),
            killed: false,
        };
        let event = Event::process_exited(&outcome, &context());
        assert_eq!(event.ty, PROCESS_EXITED);
        assert_eq!(event.data["exit_code"], json!(0));
        assert_eq!(event.data["duration_ms"], json!(250));
        assert_eq!(event.data["killed"], json!(false));
    }

    #[test]
    fn a_killed_process_records_that_it_was_killed() {
        let outcome = ProcessOutcome {
            code: None,
            duration: Duration::from_millis(10),
            killed: true,
        };
        let event = Event::process_exited(&outcome, &context());
        assert_eq!(event.data["exit_code"], Value::Null);
        assert_eq!(event.data["killed"], json!(true));
    }

    #[test]
    fn a_rule_added_event_names_the_sandbox_host_and_port() {
        let event = Event::egress_rule_added("sbx-7", &HostPort::new("api.example.com", 443));
        assert_eq!(event.ty, EGRESS_RULE_ADDED);
        assert_eq!(event.subject, "sbx-7");
        assert_eq!(event.data["sandbox_id"], json!("sbx-7"));
        assert_eq!(event.data["host"], json!("api.example.com"));
        assert_eq!(event.data["port"], json!(443));
    }

    #[test]
    fn a_rule_revoked_event_has_the_same_payload_shape() {
        let event = Event::egress_rule_revoked("sbx-7", &HostPort::new("api.example.com", 443));
        assert_eq!(event.ty, EGRESS_RULE_REVOKED);
        assert_eq!(event.data["host"], json!("api.example.com"));
        assert_eq!(event.data["port"], json!(443));
        assert_eq!(
            event.traceparent, None,
            "a rule change carries no traceparent"
        );
    }

    #[test]
    fn the_decision_events_name_the_request_and_destination() {
        let id = RequestId::new("req-9");
        let destination = HostPort::new("api.example.com", 443);
        let trace = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01".to_owned();

        let requested = Event::egress_requested("sbx-7", &id, &destination, Some(trace.clone()));
        assert_eq!(requested.ty, EGRESS_REQUESTED);
        assert_eq!(requested.subject, "sbx-7");
        assert_eq!(requested.data["sandbox_id"], json!("sbx-7"));
        assert_eq!(requested.data["request_id"], json!("req-9"));
        assert_eq!(requested.data["host"], json!("api.example.com"));
        assert_eq!(requested.data["port"], json!(443));
        assert_eq!(requested.traceparent.as_deref(), Some(trace.as_str()));

        for (event, ty) in [
            (
                Event::egress_granted("sbx-7", &id, &destination, None),
                EGRESS_GRANTED,
            ),
            (
                Event::egress_denied("sbx-7", &id, &destination, None),
                EGRESS_DENIED,
            ),
            (
                Event::egress_cancelled("sbx-7", &id, &destination, None),
                EGRESS_CANCELLED,
            ),
        ] {
            assert_eq!(event.ty, ty);
            assert_eq!(event.data["request_id"], json!("req-9"));
            assert_eq!(event.data["host"], json!("api.example.com"));
        }
    }
}
