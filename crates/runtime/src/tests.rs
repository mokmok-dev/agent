use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use agent_core::event::{Event, EventKind, FinishReason};
use agent_core::ids::{Actor, BranchId};
use agent_core::time::Timestamp;
use agent_model::{Completion, Delta, Profile, Provider, ProviderError, Usage};
use agent_store::Store;

use super::{ROOT_BRANCH, Reactor, Settings, Step, Tool, Work, draw_of};

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct Scratch {
    dir: std::path::PathBuf,
    path: std::path::PathBuf,
}

impl Scratch {
    fn new() -> Self {
        let unique = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("agent-runtime-{}-{unique}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("creates scratch dir");
        let path = dir.join("agent.db");
        Self { dir, path }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn now() -> Timestamp {
    Timestamp::new(1_790_000_000, 0).expect("valid")
}

fn later(seconds: i64) -> Timestamp {
    Timestamp::new(1_790_000_000 + seconds, 0).expect("valid")
}

struct Fake {
    pieces: Vec<&'static str>,
    failure: Option<ProviderError>,
}

impl Provider for Fake {
    fn profile(&self) -> Profile {
        Profile {
            id: "fake".to_string(),
            context_window: 8192,
            caches_prefixes: true,
            cache_ttl: None,
            reports_cached_tokens: true,
        }
    }

    fn call(
        &mut self,
        _request: &agent_model::Request,
        delta: &mut dyn FnMut(Delta<'_>),
    ) -> Result<Completion, ProviderError> {
        for piece in std::mem::take(&mut self.pieces) {
            delta(Delta::Text(piece));
        }
        self.failure.take().map_or_else(
            || {
                Ok(Completion {
                    calls: Vec::new(),
                    usage: Usage {
                        input_tokens: 3,
                        output_tokens: 2,
                        cached_tokens: Some(1),
                        estimated: false,
                    },
                    finish: Some(FinishReason::Stop),
                })
            },
            Err,
        )
    }
}

fn settings() -> Settings {
    Settings {
        model: "test-model".to_string(),
        directive_version: "v1".to_string(),
        directive: "be careful".to_string(),
        tools: vec![Tool {
            argv: vec!["cat".to_string()],
            idempotent: true,
            max_output: 4096,
            name: "shell".to_string(),
            schema: r#"{"argv":"string"}"#.to_string(),
            timeout: std::time::Duration::from_secs(5),
        }],
        max_output_tokens: 64,
    }
}

fn reactor_with_a_model(
    path: &std::path::Path,
    provider: Fake,
) -> Reactor {
    Reactor::with_model(path, Box::new(provider), settings()).expect("opens")
}

fn answer() -> Fake {
    Fake {
        pieces: vec!["h", "i"],
        failure: None,
    }
}

fn send(
    store: &mut Store,
    branch: BranchId,
    text: &str,
) {
    let events = store.load(branch).expect("loads");
    let depth = u64::try_from(events.len()).expect("small");
    let prev = events.last().map(Event::id);
    let event = Event {
        branch,
        depth,
        prev,
        time: now(),
        actor: Actor::Human("ana".to_string()),
        causation: None,
        correlation: None,
        model: None,
        kind: EventKind::CommandReceived {
            command: "send".to_string(),
            command_id: "c-1".to_string(),
            surface: "test".to_string(),
            text: Some(text.to_string()),
        },
    };
    store.append(&event).expect("appends");
}

fn seed(scratch: &Scratch) {
    let mut reactor = Reactor::open(&scratch.path).expect("opens");
    reactor.start(now()).expect("starts");
    let mut store = Store::open(&scratch.path).expect("opens");
    send(&mut store, ROOT_BRANCH, "hello");
}

fn events(scratch: &Scratch) -> Vec<Event> {
    let store = Store::open(&scratch.path).expect("opens");
    store.load(ROOT_BRANCH).expect("loads")
}

#[test]
fn a_new_store_gets_a_root_branch_and_a_start_event() {
    let scratch = Scratch::new();
    let mut reactor = Reactor::open(&scratch.path).expect("opens");
    assert_eq!(reactor.start(now()).expect("starts"), Work::Done);
    let state = reactor.state().expect("folds");
    let root = state.branch(ROOT_BRANCH).expect("root exists");
    assert_eq!(root.depth, 1);
    assert_eq!(root.label.as_deref(), Some("main"));
}

#[test]
fn starting_twice_changes_nothing() {
    let scratch = Scratch::new();
    let mut reactor = Reactor::open(&scratch.path).expect("opens");
    reactor.start(now()).expect("starts");
    assert_eq!(reactor.start(now()).expect("starts"), Work::Nothing);
    assert_eq!(events(&scratch).len(), 2);
}

#[test]
fn a_pending_send_becomes_a_turn() {
    let scratch = Scratch::new();
    seed(&scratch);
    let mut reactor = Reactor::open(&scratch.path).expect("opens");
    assert!(matches!(
        reactor.step(now()).expect("steps"),
        Step::Append(_)
    ));
    let turn = events(&scratch).last().expect("an event").clone();
    match &turn.kind {
        EventKind::TurnRequested { input, .. } => assert_eq!(input, "hello"),
        other => panic!("unexpected {other:?}"),
    }
    assert!(turn.causation.is_some());
    assert!(turn.correlation.is_some());
}

#[test]
fn a_turn_is_sent_and_answered() {
    let scratch = Scratch::new();
    seed(&scratch);
    let mut reactor = reactor_with_a_model(&scratch.path, answer());
    reactor.step(now()).expect("writes the turn");
    assert!(matches!(
        reactor.step(now()).expect("sends"),
        Step::RequestModel { .. }
    ));
    let written = events(&scratch);
    assert_eq!(written.len(), 6, "{written:?}");
    match &written[4].kind {
        EventKind::ModelRequested {
            segments,
            tokens,
            turn,
            warm,
            ..
        } => {
            assert_eq!(
                *segments, 3,
                "the directive, the tool block, and the turn's input"
            );
            assert!(*tokens > 0);
            assert!(!*warm);
            assert_eq!(Some(*turn), written[5].correlation);
        },
        other => panic!("unexpected {other:?}"),
    }
    match &written[5].kind {
        EventKind::ModelResponded {
            finish,
            input_tokens,
            output,
            output_tokens,
            ..
        } => {
            assert_eq!(*finish, FinishReason::Stop);
            assert_eq!(*input_tokens, 3);
            assert_eq!(output, "hi");
            assert_eq!(*output_tokens, 2);
        },
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn a_retryable_failure_writes_the_failure_and_the_instant_to_try_again() {
    let scratch = Scratch::new();
    seed(&scratch);
    let mut reactor = reactor_with_a_model(
        &scratch.path,
        Fake {
            pieces: Vec::new(),
            failure: Some(ProviderError::Status(500)),
        },
    );
    reactor.step(now()).expect("writes the turn");
    reactor.step(now()).expect("fails");
    let written = events(&scratch);
    assert_eq!(written.len(), 7, "{written:?}");
    match &written[5].kind {
        EventKind::ModelFailed {
            attempt, retryable, ..
        } => {
            assert_eq!(*attempt, 1);
            assert!(retryable);
        },
        other => panic!("unexpected {other:?}"),
    }
    match &written[6].kind {
        EventKind::ScheduleArmed { attempt, due, .. } => {
            assert_eq!(*attempt, 1);
            assert!(*due > now(), "a retry is owed in the future: {due:?}");
            assert!(*due < later(2), "the first backoff is under a second");
        },
        other => panic!("unexpected {other:?}"),
    }
    assert!(matches!(
        reactor.step(now()).expect("waits"),
        Step::Wait { .. }
    ));
}

#[test]
fn a_failure_that_may_not_be_retried_writes_no_schedule() {
    let scratch = Scratch::new();
    seed(&scratch);
    let mut reactor = reactor_with_a_model(
        &scratch.path,
        Fake {
            pieces: Vec::new(),
            failure: Some(ProviderError::Status(400)),
        },
    );
    reactor.step(now()).expect("writes the turn");
    reactor.step(now()).expect("fails");
    let written = events(&scratch);
    assert_eq!(written.len(), 6, "{written:?}");
    match &written[5].kind {
        EventKind::ModelFailed { retryable, .. } => assert!(!retryable),
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(reactor.step(now()).expect("idles"), Step::Idle);
}

#[test]
fn a_turn_with_no_model_configured_is_an_error() {
    let scratch = Scratch::new();
    seed(&scratch);
    let mut reactor = Reactor::open(&scratch.path).expect("opens");
    reactor.step(now()).expect("writes the turn");
    assert!(reactor.step(now()).is_err());
}

#[test]
fn the_store_still_verifies_after_the_reactor_writes() {
    let scratch = Scratch::new();
    seed(&scratch);
    let mut reactor = reactor_with_a_model(&scratch.path, answer());
    reactor.step(now()).expect("writes the turn");
    reactor.step(now()).expect("answers");
    let store = Store::open(&scratch.path).expect("opens");
    let report = store.verify().expect("verifies");
    assert_eq!(report.events, 6);
    assert_eq!(report.branches, 1);
}

#[test]
fn the_state_is_the_same_whether_it_is_folded_once_or_twice() {
    let scratch = Scratch::new();
    seed(&scratch);
    let mut reactor = reactor_with_a_model(&scratch.path, answer());
    reactor.step(now()).expect("writes the turn");
    reactor.step(now()).expect("answers");
    assert_eq!(
        reactor.state().expect("folds"),
        reactor.state().expect("folds")
    );
}

#[test]
fn an_event_for_an_unknown_branch_surfaces_as_a_fold_error() {
    let scratch = Scratch::new();
    seed(&scratch);
    let mut store = Store::open(&scratch.path).expect("opens");
    let orphan = Event {
        branch: BranchId::from_bytes([9; 8]),
        depth: 0,
        prev: None,
        time: now(),
        actor: Actor::Process("test".to_string()),
        causation: None,
        correlation: None,
        model: None,
        kind: EventKind::InstanceStopping {
            reason: "test".to_string(),
        },
    };
    store.append(&orphan).expect("appends");
    let reactor = Reactor::open(&scratch.path).expect("opens");
    assert!(reactor.state().is_err());
}

#[test]
fn the_jitter_draw_comes_from_the_request_id_so_a_replay_repeats_it() {
    let event = agent_core::ids::EventId::from_bytes([7; 32]);
    assert_eq!(draw_of(event), draw_of(event));
    assert_eq!(draw_of(event), u64::from_le_bytes([7; 8]));
}

#[test]
fn every_error_explains_itself() {
    use agent_core::fold::FoldError;
    assert_ne!(
        super::ReactorError::Fold(FoldError::UnknownBranch(ROOT_BRANCH)).to_string(),
        ""
    );
    assert_ne!(super::ReactorError::NoModel.to_string(), "");
}

#[test]
fn a_wait_is_capped_so_a_clock_jump_cannot_park_the_daemon() {
    assert_eq!(
        super::wait_for(now(), later(600)),
        Duration::from_secs(5),
        "an hour away still sleeps at most the cap"
    );
}

/// A reactor whose settings name one tool, `cat`, and nothing else.
fn reactor_with_tools(
    path: &std::path::Path,
    provider: impl Provider + 'static,
) -> Reactor {
    let mut with_tools = settings();
    with_tools.tools = vec![Tool {
        argv: vec!["cat".to_string()],
        idempotent: true,
        max_output: 4096,
        name: "shell".to_string(),
        schema: r#"{"argv":"string"}"#.to_string(),
        timeout: std::time::Duration::from_secs(5),
    }];
    Reactor::with_model(path, Box::new(provider), with_tools).expect("opens")
}

/// A provider that answers by asking for the tool named `shell`.
struct Asking;

impl agent_model::Provider for Asking {
    fn profile(&self) -> Profile {
        Profile {
            id: "asking".to_string(),
            context_window: 8192,
            caches_prefixes: true,
            cache_ttl: None,
            reports_cached_tokens: true,
        }
    }

    fn call(
        &mut self,
        _request: &agent_model::Request,
        _delta: &mut dyn FnMut(agent_model::Delta<'_>),
    ) -> Result<agent_model::Completion, agent_model::ProviderError> {
        Ok(agent_model::Completion {
            usage: agent_model::Usage {
                input_tokens: 1,
                output_tokens: 1,
                cached_tokens: None,
                estimated: false,
            },
            finish: Some(agent_core::event::FinishReason::ToolCalls),
            calls: vec![agent_model::openai::Call {
                id: "c1".to_string(),
                name: "shell".to_string(),
                arguments: r#"{"cmd":"date"}"#.to_string(),
            }],
        })
    }
}

#[test]
fn a_tool_call_is_run_and_what_it_wrote_is_recorded() {
    let scratch = Scratch::new();
    let mut reactor = Reactor::open(&scratch.path).expect("opens");
    reactor.start(now()).expect("starts");
    let mut store = Store::open(&scratch.path).expect("opens");
    send(&mut store, ROOT_BRANCH, "what time is it");
    drop(store);

    let mut reactor = reactor_with_tools(&scratch.path, Asking);
    reactor.start(now()).expect("starts");
    // Drive the loop until it has nothing left to do.
    for _ in 0..12 {
        let step = reactor.step(now()).expect("steps");
        if matches!(step, Step::Idle) {
            break;
        }
    }

    let written = events(&scratch);
    let kinds: Vec<&str> = written.iter().map(|event| event.kind.type_name()).collect();
    assert!(kinds.contains(&"dev.mokmok.agent.tool.result"), "{kinds:?}");
    let result = written
        .iter()
        .find(|event| matches!(event.kind, EventKind::ToolResult { .. }))
        .expect("a result");
    match &result.kind {
        EventKind::ToolResult {
            output,
            status,
            truncated,
            ..
        } => {
            assert_eq!(*status, 0);
            assert!(!truncated);
            assert_eq!(output.trim(), r#"{"cmd":"date"}"#);
        },
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn a_tool_that_is_not_configured_is_a_failure_rather_than_a_crash() {
    let scratch = Scratch::new();
    let mut reactor = Reactor::open(&scratch.path).expect("opens");
    reactor.start(now()).expect("starts");
    let mut store = Store::open(&scratch.path).expect("opens");
    send(&mut store, ROOT_BRANCH, "run something else");
    drop(store);

    let mut without_tools = settings();
    without_tools.tools.clear();
    let mut reactor =
        Reactor::with_model(&scratch.path, Box::new(Asking), without_tools).expect("opens");
    reactor.start(now()).expect("starts");
    for _ in 0..12 {
        let step = reactor.step(now()).expect("steps");
        if matches!(step, Step::Idle) {
            break;
        }
    }
    let written = events(&scratch);
    assert!(
        written
            .iter()
            .any(|event| matches!(event.kind, EventKind::ToolFailed { .. })),
        "a missing tool is a failure"
    );
}
