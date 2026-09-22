//! The sandbox's `CloudEvents`: every permission decision and execution
//! terminal state is appended to the event log.

use agentd_events::Event;
use serde_json::json;

/// A command or resource access needs a decision.
pub const PERMISSION_REQUESTED: &str = "sandbox.permission.requested";
/// A static policy rule allowed the access.
pub const PERMISSION_GRANTED: &str = "sandbox.permission.granted";
/// A static policy rule refused the access; deny wins.
pub const PERMISSION_DENIED: &str = "sandbox.permission.denied";
/// Nobody decided the access in time, so it was withdrawn.
pub const PERMISSION_CANCELLED: &str = "sandbox.permission.cancelled";
/// Terminal state of an execution: exit code, duration, output sizes.
pub const EXEC_COMPLETED: &str = "sandbox.exec.completed";
/// A long-lived confined process was spawned.
pub const PROCESS_STARTED: &str = "sandbox.process.started";
/// Terminal state of a long-lived confined process: exit code and duration.
pub const PROCESS_EXITED: &str = "sandbox.process.exited";

/// The decision value on `requested` events decided immediately by a static
/// rule. Human approval arrives in a later iteration; the correlation id is
/// already part of the contract.
pub const DECISION_AUTO: &str = "auto";
/// The decision value on `requested` events awaiting an approver.
pub const DECISION_PENDING: &str = "pending";

/// The resource kind every sandbox event refers to in this version.
pub const RESOURCE_SHELL: &str = "shell";
/// The action kind every sandbox event refers to in this version.
pub const ACTION_EXEC: &str = "exec";

fn permission_data(
    sandbox_id: &str,
    request_id: &str,
    agent_id: &str,
    command: &str,
    decision: &str,
) -> serde_json::Value {
    json!({
        "sandbox_id": sandbox_id,
        "request_id": request_id,
        "agent_id": agent_id,
        "resource": RESOURCE_SHELL,
        "action": ACTION_EXEC,
        "command": command,
        "decision": decision,
    })
}

/// Builds a `sandbox.permission.requested` event.
#[must_use]
pub fn permission_requested(
    sandbox_id: &str,
    request_id: &str,
    agent_id: &str,
    command: &str,
    decision: &str,
) -> Event {
    Event::new(
        PERMISSION_REQUESTED,
        permission_data(sandbox_id, request_id, agent_id, command, decision),
    )
}

/// Builds a `sandbox.permission.granted` event.
#[must_use]
pub fn permission_granted(
    sandbox_id: &str,
    request_id: &str,
    agent_id: &str,
    command: &str,
) -> Event {
    Event::new(
        PERMISSION_GRANTED,
        permission_data(sandbox_id, request_id, agent_id, command, "granted"),
    )
}

/// Builds a `sandbox.permission.denied` event.
#[must_use]
pub fn permission_denied(
    sandbox_id: &str,
    request_id: &str,
    agent_id: &str,
    command: &str,
) -> Event {
    Event::new(
        PERMISSION_DENIED,
        permission_data(sandbox_id, request_id, agent_id, command, "denied"),
    )
}

/// Builds a `sandbox.permission.cancelled` event: nobody decided the request, so
/// the sandbox withdrew it.
#[must_use]
pub fn permission_cancelled(
    sandbox_id: &str,
    request_id: &str,
    agent_id: &str,
    command: &str,
) -> Event {
    Event::new(
        PERMISSION_CANCELLED,
        permission_data(sandbox_id, request_id, agent_id, command, "cancelled"),
    )
}

/// Builds a `sandbox.exec.completed` event.
///
/// The argument count is the event contract; a struct would not shrink it.
#[expect(
    clippy::too_many_arguments,
    reason = "the eight fields are the event contract, spelled out at the call site"
)]
#[must_use]
pub fn exec_completed(
    sandbox_id: &str,
    request_id: &str,
    agent_id: &str,
    command: &str,
    exit_code: i32,
    duration_ms: u64,
    stdout_bytes: u64,
    stderr_bytes: u64,
) -> Event {
    let data = json!({
        "sandbox_id": sandbox_id,
        "request_id": request_id,
        "agent_id": agent_id,
        "resource": RESOURCE_SHELL,
        "action": ACTION_EXEC,
        "command": command,
        "exit_code": exit_code,
        "duration_ms": duration_ms,
        "stdout_bytes": stdout_bytes,
        "stderr_bytes": stderr_bytes,
    });
    Event::new(EXEC_COMPLETED, data)
}

/// Builds a `sandbox.process.started` event.
#[must_use]
pub fn process_started(
    sandbox_id: &str,
    request_id: &str,
    agent_id: &str,
    command: &str,
    process_id: &str,
) -> Event {
    let data = json!({
        "sandbox_id": sandbox_id,
        "request_id": request_id,
        "process_id": process_id,
        "agent_id": agent_id,
        "resource": RESOURCE_SHELL,
        "action": ACTION_EXEC,
        "command": command,
    });
    Event::new(PROCESS_STARTED, data)
}

/// Builds a `sandbox.process.exited` event.
#[must_use]
pub fn process_exited(
    sandbox_id: &str,
    request_id: &str,
    agent_id: &str,
    command: &str,
    process_id: &str,
    exit_code: i32,
    duration_ms: u64,
) -> Event {
    let data = json!({
        "sandbox_id": sandbox_id,
        "request_id": request_id,
        "process_id": process_id,
        "agent_id": agent_id,
        "resource": RESOURCE_SHELL,
        "action": ACTION_EXEC,
        "command": command,
        "exit_code": exit_code,
        "duration_ms": duration_ms,
    });
    Event::new(PROCESS_EXITED, data)
}

#[cfg(test)]
mod tests {
    use super::{
        ACTION_EXEC, DECISION_AUTO, EXEC_COMPLETED, PERMISSION_CANCELLED, PERMISSION_DENIED,
        PERMISSION_GRANTED, PERMISSION_REQUESTED, RESOURCE_SHELL, exec_completed,
        permission_cancelled, permission_denied, permission_granted, permission_requested,
    };
    use agentd_events::{DAEMON_SOURCE, SPEC_VERSION};
    use serde_json::json;

    #[test]
    fn requested_event_carries_the_contract_fields() {
        let event = permission_requested("sbx-1", "req-1", "coder-1", "cargo test", DECISION_AUTO);

        assert_eq!(event.r#type, PERMISSION_REQUESTED);
        assert_eq!(event.source, DAEMON_SOURCE);
        assert_eq!(event.specversion, SPEC_VERSION);
        assert_eq!(
            event.data,
            json!({
                "sandbox_id": "sbx-1",
                "request_id": "req-1",
                "agent_id": "coder-1",
                "resource": RESOURCE_SHELL,
                "action": ACTION_EXEC,
                "command": "cargo test",
                "decision": DECISION_AUTO,
            })
        );
    }

    #[test]
    fn decision_events_flip_the_type_and_decision() {
        let granted = permission_granted("sbx-1", "req-1", "coder-1", "ls");
        let denied = permission_denied("sbx-1", "req-1", "coder-1", "ls");
        let cancelled = permission_cancelled("sbx-1", "req-1", "coder-1", "ls");

        assert_eq!(granted.r#type, PERMISSION_GRANTED);
        assert_eq!(granted.data["decision"], json!("granted"));
        assert_eq!(denied.r#type, PERMISSION_DENIED);
        assert_eq!(denied.data["decision"], json!("denied"));
        assert_eq!(cancelled.r#type, PERMISSION_CANCELLED);
        assert_eq!(cancelled.data["decision"], json!("cancelled"));
    }

    #[test]
    fn completed_event_reports_the_terminal_state() {
        let event = exec_completed("sbx-1", "req-1", "coder-1", "ls", 0, 12, 32, 0);

        assert_eq!(event.r#type, EXEC_COMPLETED);
        assert_eq!(
            event.data,
            json!({
                "sandbox_id": "sbx-1",
                "request_id": "req-1",
                "agent_id": "coder-1",
                "resource": RESOURCE_SHELL,
                "action": ACTION_EXEC,
                "command": "ls",
                "exit_code": 0,
                "duration_ms": 12,
                "stdout_bytes": 32,
                "stderr_bytes": 0,
            })
        );
    }
}
