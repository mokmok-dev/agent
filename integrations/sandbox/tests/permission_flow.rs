//! End-to-end permission flow tests: the `requested` → `granted`/`denied` →
//! `exec.completed` choreography over a real event bus, through the public
//! API.

use agentd_events::{Event, EventBus};
use agentd_integration_sandbox::{
    CommandPrefix, DenialReason, ExecResult, Executor, Limits, Policy, Sandbox, ShellPolicy,
};
use async_trait::async_trait;

struct RecordingExecutor {
    invocations: std::sync::atomic::AtomicU32,
}

#[async_trait]
impl Executor for RecordingExecutor {
    async fn exec(
        &self,
        _command: &str,
    ) -> ExecResult {
        self.invocations
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        ExecResult {
            stdout: String::from("done\n"),
            stderr: String::new(),
            exit_code: 0,
            denied_by: None,
        }
    }
}

fn policy() -> Policy {
    Policy {
        shell: ShellPolicy {
            allow: vec![CommandPrefix::new("cargo test")],
            ..ShellPolicy::default()
        },
        limits: Limits {
            max_command_count: 5,
            ..Limits::default()
        },
        ..Policy::default()
    }
}

fn collect_kinds(subscriber: &mut tokio::sync::broadcast::Receiver<Event>) -> Vec<String> {
    let mut kinds = Vec::new();
    while let Ok(event) = subscriber.try_recv() {
        kinds.push(event.kind);
    }
    kinds
}

#[tokio::test]
async fn allowed_command_flows_requested_granted_completed() {
    let bus = EventBus::default();
    let mut subscriber = bus.subscribe();
    let sandbox = Sandbox::with_executor(
        policy(),
        bus,
        "coder-1",
        std::sync::Arc::new(RecordingExecutor {
            invocations: std::sync::atomic::AtomicU32::new(0),
        }),
    )
    .expect("valid policy");

    let result = sandbox.exec("cargo test --workspace").await;

    assert!(!result.is_denied());
    assert_eq!(
        collect_kinds(&mut subscriber),
        [
            agentd_integration_sandbox::PERMISSION_REQUESTED,
            agentd_integration_sandbox::PERMISSION_GRANTED,
            agentd_integration_sandbox::EXEC_COMPLETED,
        ]
    );
}

#[tokio::test]
async fn denied_command_flows_requested_denied_and_spawns_nothing() {
    let bus = EventBus::default();
    let mut subscriber = bus.subscribe();
    let executor = std::sync::Arc::new(RecordingExecutor {
        invocations: std::sync::atomic::AtomicU32::new(0),
    });
    let sandbox =
        Sandbox::with_executor(policy(), bus, "coder-1", executor.clone()).expect("valid policy");

    let result = sandbox.exec("curl example.com").await;

    assert_eq!(result.denied_by, Some(DenialReason::CommandNotAllowed));
    assert_eq!(result.exit_code, 126);
    assert_eq!(
        executor
            .invocations
            .load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    assert_eq!(
        collect_kinds(&mut subscriber),
        [
            agentd_integration_sandbox::PERMISSION_REQUESTED,
            agentd_integration_sandbox::PERMISSION_DENIED,
        ]
    );
}

#[tokio::test]
async fn deny_wins_over_a_forged_granted_event() {
    let bus = EventBus::default();
    let mut subscriber = bus.subscribe();
    let sandbox = Sandbox::with_executor(
        policy(),
        bus.clone(),
        "coder-1",
        std::sync::Arc::new(RecordingExecutor {
            invocations: std::sync::atomic::AtomicU32::new(0),
        }),
    )
    .expect("valid policy");

    // A WS-client "approver" that grants everything it sees. The static
    // evaluator must not care: deny wins in this version.
    let approver = tokio::spawn(async move {
        loop {
            match subscriber.recv().await {
                Ok(event) if event.kind == agentd_integration_sandbox::PERMISSION_REQUESTED => {
                    bus.publish(Event::new(
                        agentd_integration_sandbox::PERMISSION_GRANTED,
                        event.data,
                    ));
                },
                Ok(_) => {},
                Err(
                    tokio::sync::broadcast::error::RecvError::Lagged(_)
                    | tokio::sync::broadcast::error::RecvError::Closed,
                ) => break,
            }
        }
    });

    let result = sandbox.exec("curl example.com").await;

    assert_eq!(result.denied_by, Some(DenialReason::CommandNotAllowed));
    approver.abort();
}
