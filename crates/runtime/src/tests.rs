use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_core::event::{Event, EventKind, FinishReason};
use agent_core::fold::FoldError;
use agent_core::fork::{ForkError, Selector};
use agent_core::ids::{Actor, BranchId, CallId, EventId, TurnId};
use agent_core::time::Timestamp;
use agent_model::{Completion, Delta, Profile, Provider, ProviderError, Usage};
use agent_store::Store;

use super::{
    Gate, Network, ROOT_BRANCH, Reactor, ReactorError, Settings, Step, Tool, Work, draw_of, gate,
};
use crate::bus::{Command, ForkRequest, Refusal, Reply};
use crate::route::route;
use agent_core::event::Risk;

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

fn call() -> CallId {
    CallId {
        turn: TurnId::from_bytes([9; 8]),
        step: 0,
    }
}

fn settings() -> Settings {
    Settings {
        approval_grace: std::time::Duration::from_secs(60),
        model: "test-model".to_string(),
        directive_version: "v1".to_string(),
        directive: "be careful".to_string(),
        workspace: std::env::temp_dir(),
        tools: vec![Tool {
            description: String::new(),
            risk: agent_core::event::Risk::Read,
            argv: vec!["cat".to_string()],
            idempotent: true,
            max_output: 4096,
            name: "shell".to_string(),
            schema: r#"{"argv":"string"}"#.to_string(),
            timeout: std::time::Duration::from_secs(5),
            env: Vec::new(),
            write: Vec::new(),
            network: Network::Any,
            memory_bytes: 256 * 1024 * 1024,
            file_bytes: 8 * 1024 * 1024,
            confined: false,
        }],
        max_output_tokens: 64,
    }
}

fn reactor_with_a_model(
    path: &std::path::Path,
    provider: impl Provider + 'static,
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
    send_named(store, branch, "send", text);
}

/// A command from a person, whatever it asks for.
fn send_named(
    store: &mut Store,
    branch: BranchId,
    command: &str,
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
            command: command.to_string(),
            command_id: format!("c-{depth}"),
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

fn started(scratch: &Scratch) -> Reactor {
    let mut reactor = Reactor::open(&scratch.path).expect("opens");
    reactor.start(now()).expect("starts");
    reactor
}

fn fork_at(selector: Selector) -> ForkRequest {
    ForkRequest {
        parent: ROOT_BRANCH,
        selector,
        label: None,
        boundary: false,
        force: false,
    }
}

fn total_events(scratch: &Scratch) -> usize {
    let store = Store::open(&scratch.path).expect("opens");
    store
        .branches()
        .expect("lists")
        .into_iter()
        .map(|branch| store.load(branch).expect("loads").len())
        .sum()
}

fn append_message(
    store: &mut Store,
    branch: BranchId,
    content: &str,
) -> EventId {
    let existing = store.load(branch).expect("loads");
    let depth = existing.last().map_or(0, |event| event.depth + 1);
    let event = Event {
        branch,
        depth,
        prev: existing.last().map(Event::id),
        time: now(),
        actor: Actor::Human("ana".to_string()),
        causation: None,
        correlation: None,
        model: None,
        kind: EventKind::MessageAppended {
            content: content.to_string(),
            role: agent_core::event::Role::Assistant,
            tokens: 1,
        },
    };
    store.append(&event).expect("appends");
    event.id()
}

/// The root after one complete turn: its request, its answer, and the answer
/// filed into the transcript.
fn seeded_turn(scratch: &Scratch) -> (Reactor, Vec<Event>, TurnId) {
    seed(scratch);
    let mut reactor = reactor_with_a_model(&scratch.path, answer());
    for _ in 0..4 {
        if matches!(reactor.step(now()).expect("steps"), Step::Idle) {
            break;
        }
    }
    let events = events(scratch);
    let turn = events
        .iter()
        .find_map(|event| match &event.kind {
            EventKind::TurnRequested { turn, .. } => Some(*turn),
            _ => None,
        })
        .expect("the turn was requested");
    (reactor, events, turn)
}

/// A provider that records every request it is given.
struct Capturing {
    seen: Arc<Mutex<Vec<agent_model::Request>>>,
}

impl Provider for Capturing {
    fn profile(&self) -> Profile {
        Profile {
            id: "capturing".to_string(),
            context_window: 8192,
            caches_prefixes: true,
            cache_ttl: None,
            reports_cached_tokens: true,
        }
    }

    fn call(
        &mut self,
        request: &agent_model::Request,
        delta: &mut dyn FnMut(Delta<'_>),
    ) -> Result<Completion, ProviderError> {
        self.seen
            .lock()
            .expect("the capture lock")
            .push(request.clone());
        delta(Delta::Text("child answer"));
        Ok(Completion {
            calls: Vec::new(),
            usage: Usage {
                input_tokens: 3,
                output_tokens: 2,
                cached_tokens: None,
                estimated: false,
            },
            finish: Some(FinishReason::Stop),
        })
    }
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

/// A provider that never answers, so only the retry budget can end the turn.
struct Refusing;

impl Provider for Refusing {
    fn profile(&self) -> Profile {
        Profile {
            id: "refusing".to_string(),
            context_window: 8192,
            caches_prefixes: true,
            cache_ttl: None,
            reports_cached_tokens: true,
        }
    }

    fn call(
        &mut self,
        _request: &agent_model::Request,
        _delta: &mut dyn FnMut(Delta<'_>),
    ) -> Result<Completion, ProviderError> {
        Err(ProviderError::Transport(String::from("refused")))
    }
}

#[test]
fn an_exhausted_retry_budget_stops_asking_instead_of_asking_forever() {
    // Past the budget there is no instant the next attempt is owed, so a request
    // left pending turns every iteration into a new call. Measured against a
    // refused connection: 840 failures in eight seconds, and the log growing by a
    // megabyte while nobody learns anything.
    let scratch = Scratch::new();
    seed(&scratch);
    let mut reactor = reactor_with_a_model(&scratch.path, Refusing);
    reactor.step(now()).expect("writes the turn");
    let mut asked = 0_u32;
    let mut at = now();
    for _ in 0..10 {
        match reactor.step(at).expect("steps") {
            Step::RequestModel { .. } => {
                asked = asked.saturating_add(1);
                at = at.plus(Duration::from_secs(60)).expect("valid");
            },
            Step::Wait { until } => at = until,
            Step::Idle => break,
            Step::Append(_) | Step::Execute { .. } => {},
        }
    }
    assert_eq!(
        asked,
        agent_model::MAX_ATTEMPTS,
        "the budget is the number of attempts made, and no more"
    );
    let written = events(&scratch);
    let failures: Vec<&Event> = written
        .iter()
        .filter(|event| matches!(event.kind, EventKind::ModelFailed { .. }))
        .collect();
    assert_eq!(failures.len(), 3, "{written:?}");
    match &failures.last().expect("a failure").kind {
        EventKind::ModelFailed { retryable, .. } => {
            assert!(!retryable, "the last attempt is not retried, and says so");
        },
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(
        reactor.step(at).expect("idles"),
        Step::Idle,
        "an ended turn waits for a command rather than asking again"
    );
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
    let tip = store
        .load(ROOT_BRANCH)
        .expect("loads")
        .last()
        .expect("a tip")
        .clone();
    // Anchored to the root's tip, so the store accepts the position and the fold
    // is the layer that reports the branch it was never told about.
    let orphan = Event {
        branch: BranchId::from_bytes([9; 8]),
        depth: tip.depth + 1,
        prev: Some(tip.id()),
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
    assert_ne!(
        ReactorError::Fold(FoldError::UnknownBranch(ROOT_BRANCH)).to_string(),
        ""
    );
    assert_ne!(ReactorError::NoModel.to_string(), "");
    assert_ne!(ReactorError::NoSuchBranch(ROOT_BRANCH).to_string(), "");
    assert_ne!(ReactorError::Fork(ForkError::BeforeGenesis).to_string(), "");
    assert_ne!(
        ReactorError::ForkStranded {
            call: call(),
            tool: "shell".to_string(),
        }
        .to_string(),
        ""
    );
}

#[test]
fn a_wait_is_capped_so_a_clock_jump_cannot_park_the_daemon() {
    assert_eq!(
        super::wait_for(now(), later(600)),
        Duration::from_secs(5),
        "an hour away still sleeps at most the cap"
    );
}

#[test]
fn a_wait_shorter_than_a_second_is_not_a_wait_of_none() {
    // The backoff's first ceiling is under a second, so the common retry is
    // exactly the case whole-second arithmetic rounds down to zero.
    let due = now().plus(Duration::from_millis(400)).expect("valid");
    assert_eq!(super::wait_for(now(), due), Duration::from_millis(400));
    let past = now().plus(Duration::from_millis(1)).expect("valid");
    assert_eq!(
        super::wait_for(past, now()),
        Duration::ZERO,
        "an instant already gone is not a wait"
    );
}

/// A reactor whose settings name one tool, `cat`, and nothing else.
fn reactor_with_tools(
    path: &std::path::Path,
    provider: impl Provider + 'static,
) -> Reactor {
    let mut with_tools = settings();
    with_tools.tools = vec![Tool {
        description: String::new(),
        risk: agent_core::event::Risk::Read,
        argv: vec!["cat".to_string()],
        idempotent: true,
        max_output: 4096,
        name: "shell".to_string(),
        schema: r#"{"argv":"string"}"#.to_string(),
        timeout: std::time::Duration::from_secs(5),
        env: Vec::new(),
        write: Vec::new(),
        network: Network::Any,
        memory_bytes: 256 * 1024 * 1024,
        file_bytes: 8 * 1024 * 1024,
        confined: false,
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
fn an_intent_names_the_environment_its_tool_declares() {
    let scratch = Scratch::new();
    let mut with_env = settings();
    let mut tool = marker_tool(&scratch, true);
    tool.env = vec!["PATH".to_string()];
    with_env.tools = vec![tool];
    let mut reactor =
        Reactor::with_model(&scratch.path, Box::new(Asking), with_env).expect("opens");
    reactor.start(now()).expect("starts");
    let mut store = Store::open(&scratch.path).expect("opens");
    send(&mut store, ROOT_BRANCH, "what time is it");
    drop(store);

    for _ in 0..12 {
        let step = reactor.step(now()).expect("steps");
        if matches!(step, Step::Idle) {
            break;
        }
    }

    let named = events(&scratch)
        .iter()
        .find_map(|event| match &event.kind {
            EventKind::ToolIntent { env, .. } => Some(env.clone()),
            _ => None,
        })
        .expect("an intent");
    assert_eq!(
        named,
        vec!["PATH".to_string()],
        "the log names the environment the tool declared, never its values"
    );
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

/// Leaves the store in the state a process that died mid-call would leave: a turn
/// that asked for a tool, an intent, and no result.
fn left_behind(
    scratch: &Scratch,
    tool: Tool,
) {
    let mut with_tool = settings();
    with_tool.tools = vec![tool];
    let mut reactor =
        Reactor::with_model(&scratch.path, Box::new(Asking), with_tool).expect("opens");
    reactor.start(now()).expect("starts");
    let mut store = Store::open(&scratch.path).expect("opens");
    send(&mut store, ROOT_BRANCH, "do the thing");
    drop(store);

    let mut without_tools = settings();
    without_tools.tools = Vec::new();
    let mut reactor =
        Reactor::with_model(&scratch.path, Box::new(Asking), without_tools).expect("opens");
    reactor.start(now()).expect("starts");
    // The turn, the answer, and the intent. The fourth step would run it, and this
    // process is the one that is about to die.
    for _ in 0..3 {
        reactor.step(now()).expect("steps");
    }
}

fn marker_tool(
    scratch: &Scratch,
    idempotent: bool,
) -> Tool {
    marker_tool_of(scratch, Risk::Read, idempotent)
}

fn destructive_marker_tool(
    scratch: &Scratch,
    idempotent: bool,
) -> Tool {
    marker_tool_of(scratch, Risk::Destructive, idempotent)
}

fn marker_tool_of(
    scratch: &Scratch,
    risk: Risk,
    idempotent: bool,
) -> Tool {
    let marker = scratch.path.with_extension("ran");
    Tool {
        description: String::new(),
        risk,
        argv: vec![
            "sh".to_string(),
            "-c".to_string(),
            format!("echo ran >> {}", marker.display()),
        ],
        idempotent,
        max_output: 4096,
        name: "shell".to_string(),
        schema: r#"{"argv":"string"}"#.to_string(),
        timeout: std::time::Duration::from_secs(5),
        env: Vec::new(),
        // The marker is the whole point of this tool, and a tool writes where its
        // manifest says it may and nowhere else.
        write: vec![scratch.dir.clone()],
        network: Network::Any,
        memory_bytes: 256 * 1024 * 1024,
        file_bytes: 8 * 1024 * 1024,
        confined: false,
    }
}

#[test]
fn a_call_left_behind_by_a_dead_process_is_not_run_again_when_it_is_not_idempotent() {
    let scratch = Scratch::new();
    let tool = marker_tool(&scratch, false);
    let marker = scratch.path.with_extension("ran");
    left_behind(&scratch, tool.clone());

    let mut settings = settings();
    settings.tools = vec![tool];
    let mut reactor =
        Reactor::with_model(&scratch.path, Box::new(Asking), settings).expect("opens");
    reactor.start(now()).expect("starts");
    let _ = reactor.step(now()).expect("steps");

    assert!(
        !marker.exists(),
        "a tool that may not run twice was run anyway"
    );
    let written = events(&scratch);
    let failure = written
        .iter()
        .find_map(|event| match &event.kind {
            EventKind::ToolFailed {
                may_have_happened, ..
            } => Some(*may_have_happened),
            _ => None,
        })
        .expect("the log says the call is over");
    assert!(failure, "the effect may have happened");
}

#[test]
fn a_call_left_behind_is_run_again_when_the_tool_says_it_is_idempotent() {
    let scratch = Scratch::new();
    let tool = marker_tool(&scratch, true);
    let marker = scratch.path.with_extension("ran");
    left_behind(&scratch, tool.clone());

    let mut settings = settings();
    settings.tools = vec![tool];
    let mut reactor =
        Reactor::with_model(&scratch.path, Box::new(Asking), settings).expect("opens");
    reactor.start(now()).expect("starts");
    let _ = reactor.step(now()).expect("steps");

    assert!(marker.exists(), "an idempotent tool is safe to run again");
    assert!(
        events(&scratch)
            .iter()
            .any(|event| matches!(event.kind, EventKind::ToolResult { .. })),
        "the run is in the log"
    );
}

#[test]
fn a_yes_that_arrives_after_a_restart_lets_the_call_run() {
    let scratch = Scratch::new();
    let marker = scratch.path.with_extension("ran");

    // The process that asked the question, stopped before anyone answered it.
    let mut asking = settings();
    asking.tools = vec![destructive_marker_tool(&scratch, false)];
    let mut reactor = Reactor::with_model(&scratch.path, Box::new(Asking), asking).expect("opens");
    reactor.start(now()).expect("starts");
    let mut store = Store::open(&scratch.path).expect("opens");
    send(&mut store, ROOT_BRANCH, "do the thing");
    drop(store);
    for _ in 0..8 {
        if matches!(reactor.step(now()).expect("steps"), Step::Wait { .. }) {
            break;
        }
    }
    drop(reactor);

    let waiting = events(&scratch)
        .into_iter()
        .find_map(|event| match event.kind {
            EventKind::ApprovalRequested { call, .. } => Some(call),
            _ => None,
        })
        .expect("the log carries the question");
    assert!(!marker.exists(), "nothing runs before the answer");

    // A later process, which is the one that hears the yes.
    let mut answering = settings();
    answering.tools = vec![destructive_marker_tool(&scratch, false)];
    let mut reactor =
        Reactor::with_model(&scratch.path, Box::new(Asking), answering).expect("opens");
    reactor.start(now()).expect("starts");
    reactor
        .command(ROOT_BRANCH, "approve", &waiting.to_string(), now())
        .expect("takes the answer");
    for _ in 0..12 {
        if matches!(reactor.step(now()).expect("steps"), Step::Idle) {
            break;
        }
    }

    assert!(
        marker.exists(),
        "a yes written by this process is the first yes this call ever had"
    );
    let written = events(&scratch);
    assert!(
        written
            .iter()
            .any(|event| matches!(event.kind, EventKind::ToolResult { .. })),
        "the run is in the log"
    );
    assert!(
        !written.iter().any(|event| matches!(
            event.kind,
            EventKind::ToolFailed {
                may_have_happened: true,
                ..
            }
        )),
        "nothing claims an effect that never happened"
    );
}

#[test]
fn a_call_this_process_filed_runs_even_when_the_tool_may_not_repeat() {
    let scratch = Scratch::new();
    let tool = marker_tool(&scratch, false);
    let marker = scratch.path.with_extension("ran");

    let mut with_tool = settings();
    with_tool.tools = vec![tool];
    let mut reactor = Reactor::open(&scratch.path).expect("opens");
    reactor.start(now()).expect("starts");
    let mut store = Store::open(&scratch.path).expect("opens");
    send(&mut store, ROOT_BRANCH, "do the thing");
    drop(store);
    drop(reactor);

    // A fresh process, which files the intent itself and then runs it.
    let mut with_tool = settings();
    with_tool.tools = vec![marker_tool(&scratch, false)];
    let mut reactor =
        Reactor::with_model(&scratch.path, Box::new(Asking), with_tool).expect("opens");
    reactor.start(now()).expect("starts");
    for _ in 0..10 {
        if matches!(reactor.step(now()).expect("steps"), Step::Idle) {
            break;
        }
    }

    assert!(marker.exists(), "the tool this process filed has to run");
    assert!(
        events(&scratch)
            .iter()
            .any(|event| matches!(event.kind, EventKind::ToolResult { .. })),
        "and its result is in the log"
    );
}

#[test]
fn the_gate_allows_what_needs_no_answer() {
    assert_eq!(
        gate(Risk::Read, None, false, None, call(), now()),
        Gate::Run
    );
    assert_eq!(
        gate(Risk::Write, None, false, None, call(), now()),
        Gate::Run
    );
}

#[test]
fn the_gate_asks_before_what_needs_an_answer() {
    assert_eq!(
        gate(Risk::Destructive, None, false, None, call(), now()),
        Gate::Ask
    );
    assert_eq!(
        gate(Risk::Network, None, false, None, call(), now()),
        Gate::Ask
    );
}

#[test]
fn the_gate_remembers_one_answer_for_a_network_tool() {
    assert_eq!(
        gate(Risk::Network, None, true, None, call(), now()),
        Gate::Run
    );
    assert_eq!(
        gate(Risk::Destructive, None, true, None, call(), now()),
        Gate::Ask,
        "a destructive call is asked every time"
    );
}

#[test]
fn the_gate_waits_while_the_question_is_open() {
    let later = now()
        .plus(std::time::Duration::from_secs(60))
        .expect("later");
    assert_eq!(
        gate(
            Risk::Destructive,
            None,
            false,
            Some((&call(), later)),
            call(),
            now()
        ),
        Gate::Wait
    );
}

#[test]
fn the_gate_treats_an_unanswered_question_as_a_no() {
    let past = now()
        .plus(std::time::Duration::from_secs(1))
        .expect("later");
    let after = now()
        .plus(std::time::Duration::from_secs(2))
        .expect("after");
    assert_eq!(
        gate(
            Risk::Destructive,
            None,
            false,
            Some((&call(), past)),
            call(),
            after
        ),
        Gate::Expired
    );
}

#[test]
fn the_gate_refuses_what_was_refused() {
    assert_eq!(
        gate(Risk::Read, Some(&false), false, None, call(), now()),
        Gate::Refuse
    );
    assert_eq!(
        gate(Risk::Destructive, Some(&true), false, None, call(), now()),
        Gate::Run
    );
}

/// A destructive tool that leaves a marker, and the log state just before it runs.
fn destructive_call(
    scratch: &Scratch,
    risk: Risk,
) -> Tool {
    let marker = scratch.path.with_extension("ran");
    let mut tool = marker_tool(scratch, true);
    tool.argv = vec![
        "sh".to_string(),
        "-c".to_string(),
        format!("echo ran >> {}", marker.display()),
    ];
    tool.risk = risk;
    tool
}

fn ask_for(
    scratch: &Scratch,
    tool: Tool,
) -> Reactor {
    left_behind(scratch, tool.clone());
    let mut settings = settings();
    settings.tools = vec![tool];
    let mut reactor =
        Reactor::with_model(&scratch.path, Box::new(Asking), settings).expect("opens");
    reactor.start(now()).expect("starts");
    let _ = reactor.step(now()).expect("steps");
    reactor
}

#[test]
fn a_destructive_call_asks_before_it_runs() {
    let scratch = Scratch::new();
    let marker = scratch.path.with_extension("ran");
    let _reactor = ask_for(&scratch, destructive_call(&scratch, Risk::Destructive));

    assert!(!marker.exists(), "it ran without asking");
    let written = events(&scratch);
    let asked = written
        .iter()
        .find_map(|event| match &event.kind {
            EventKind::ApprovalRequested { risk, tool, .. } => Some((*risk, tool.clone())),
            _ => None,
        })
        .expect("the log holds the question");
    assert_eq!(asked.0, Risk::Destructive);
    assert_eq!(asked.1, "shell");
    let expires = written
        .iter()
        .find_map(|event| match &event.kind {
            EventKind::ApprovalRequested { expires, .. } => Some(*expires),
            _ => None,
        })
        .expect("the question");
    let expected = now()
        .plus(std::time::Duration::from_secs(60))
        .expect("later");
    assert_eq!(expires, expected, "the grace comes from the settings");
}

#[test]
fn a_question_that_is_answered_yes_lets_the_call_run() {
    let scratch = Scratch::new();
    let marker = scratch.path.with_extension("ran");
    let mut reactor = ask_for(&scratch, destructive_call(&scratch, Risk::Destructive));

    let call = {
        let written = events(&scratch);
        written
            .iter()
            .find_map(|event| match &event.kind {
                EventKind::ToolIntent { call, .. } => Some(*call),
                _ => None,
            })
            .expect("an intent")
    };
    let mut store = Store::open(&scratch.path).expect("opens");
    send_named(&mut store, ROOT_BRANCH, "approve", &call.to_string());
    drop(store);

    for _ in 0..6 {
        if matches!(reactor.step(now()).expect("steps"), Step::Idle) {
            break;
        }
    }
    assert!(marker.exists(), "an allowed call runs");
    assert!(
        events(&scratch)
            .iter()
            .any(|event| matches!(event.kind, EventKind::ToolResult { .. })),
        "and its result is in the log"
    );
}

#[test]
fn a_question_that_expires_is_a_no_and_the_call_never_runs() {
    let scratch = Scratch::new();
    let marker = scratch.path.with_extension("ran");
    let mut reactor = ask_for(&scratch, destructive_call(&scratch, Risk::Destructive));

    let later = now()
        .plus(std::time::Duration::from_secs(3600))
        .expect("later");
    for _ in 0..4 {
        if matches!(reactor.step(later).expect("steps"), Step::Idle) {
            break;
        }
    }

    assert!(!marker.exists(), "an expired question is a no");
    let decided = events(&scratch)
        .iter()
        .find_map(|event| match &event.kind {
            EventKind::ApprovalDecided { allow, note, .. } => Some((*allow, note.clone())),
            _ => None,
        })
        .expect("the answer is recorded");
    assert!(!decided.0);
    assert_eq!(decided.1.as_deref(), Some("the question expired"));
}

#[test]
fn a_command_from_the_socket_is_answered_after_the_append() {
    let scratch = Scratch::new();
    let socket = scratch.path.with_extension("sock");
    let bus = crate::bus::Bus::bind(&socket).expect("binds");
    let mut reactor = Reactor::open(&scratch.path).expect("opens");
    reactor.start(now()).expect("starts");

    // The client is the shape `agentctl` writes, so this covers the wire as well
    // as the writer.
    let branch = ROOT_BRANCH;
    let target = socket;
    let client = std::thread::spawn(move || {
        use std::io::{BufRead as _, BufReader, Write as _};
        let mut stream = std::os::unix::net::UnixStream::connect(&target).expect("connects");
        let line =
            format!(r#"{{"command":"send","branch":"{branch}","text":"hello from the socket"}}"#);
        stream.write_all(line.as_bytes()).expect("writes");
        stream.write_all(b"\n").expect("writes");
        stream.flush().expect("flushes");
        let mut reader = BufReader::new(stream);
        let mut answer = String::new();
        reader.read_line(&mut answer).expect("reads");
        answer.trim_end().to_string()
    });

    let started = std::time::Instant::now();
    let pending = loop {
        let mut pending = bus.poll();
        if let Some(pending) = pending.pop() {
            break pending;
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "the loop never polled the socket"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    };
    let command = pending.command().expect("a command").clone();
    assert_eq!(
        command,
        Command::Send {
            branch,
            text: "hello from the socket".to_string()
        }
    );
    let reply = route(&mut reactor, &command, now());
    assert_eq!(reply, Ok(Reply::Taken));
    pending.answer(&reply);

    let answer = client.join().expect("the client finished");
    assert_eq!(answer, r#"{"ok":true}"#);
    let written = events(&scratch);
    let received = written
        .iter()
        .find_map(|event| match &event.kind {
            EventKind::CommandReceived {
                command,
                surface,
                text,
                ..
            } => Some((command.clone(), surface.clone(), text.clone())),
            _ => None,
        })
        .expect("the command is in the log");
    assert_eq!(received.0, "send");
    assert_eq!(received.1, "bus");
    assert_eq!(received.2.as_deref(), Some("hello from the socket"));
}

#[test]
fn a_fork_at_the_tip_appends_one_branch_created() {
    let scratch = Scratch::new();
    let (mut reactor, parent, _) = seeded_turn(&scratch);
    let tip = parent.last().expect("a tip").clone();
    let before = total_events(&scratch);

    let mut request = fork_at(Selector::Tip);
    request.label = Some("experiment".to_string());
    let forked = reactor.fork(&request, now()).expect("forks");

    assert_eq!(forked.parent, ROOT_BRANCH);
    assert_eq!(forked.fork, tip.id());
    assert_eq!(forked.depth, tip.depth + 1);
    assert_eq!(forked.label.as_deref(), Some("experiment"));

    let store = Store::open(&scratch.path).expect("opens");
    let child = store.load(forked.branch).expect("loads");
    assert_eq!(child.len(), 1, "a fork appends exactly one event");
    let created = &child[0];
    assert_eq!(created.id(), forked.event);
    assert_eq!(created.branch, forked.branch);
    assert_eq!(created.depth, tip.depth + 1, "one past the fork event");
    assert_eq!(created.prev, Some(tip.id()), "the fork event is the parent");
    assert_eq!(created.time, now());
    assert_eq!(created.actor, Actor::Process("reactor".to_string()));
    assert_eq!(created.causation, None);
    assert_eq!(created.correlation, None);
    assert_eq!(created.model, None);
    match &created.kind {
        EventKind::BranchCreated {
            parent: named,
            fork,
            label,
        } => {
            assert_eq!(*named, Some(ROOT_BRANCH));
            assert_eq!(*fork, Some(tip.id()));
            assert_eq!(label.as_deref(), Some("experiment"));
        },
        other => panic!("unexpected {other:?}"),
    }

    assert_eq!(
        store.load(ROOT_BRANCH).expect("loads"),
        parent,
        "the parent branch receives no event"
    );
    assert_eq!(total_events(&scratch), before + 1);
    let report = store.verify().expect("verifies");
    assert_eq!(report.events, u64::try_from(before + 1).expect("small"));
    assert_eq!(report.branches, 2);
}

#[test]
fn a_fork_at_an_event_starts_one_past_that_event() {
    let scratch = Scratch::new();
    let (mut reactor, parent, _) = seeded_turn(&scratch);
    let point = parent[3].clone();
    let forked = reactor
        .fork(&fork_at(Selector::At(point.id())), now())
        .expect("forks");
    assert_eq!(forked.fork, point.id());
    assert_eq!(forked.parent, ROOT_BRANCH);
    assert_eq!(forked.depth, point.depth + 1);
}

#[test]
fn a_rewind_counts_back_from_the_tip_and_a_turn_runs_the_turn_again() {
    let scratch = Scratch::new();
    let (mut reactor, parent, turn) = seeded_turn(&scratch);

    let rewound = reactor
        .fork(&fork_at(Selector::Rewind(1)), now())
        .expect("rewinds");
    let chosen = &parent[parent.len() - 2];
    assert_eq!(rewound.fork, chosen.id());
    assert_eq!(rewound.depth, chosen.depth + 1);

    let rerun = reactor
        .fork(&fork_at(Selector::Turn(turn)), now())
        .expect("turns");
    let requested = parent
        .iter()
        .position(|event| matches!(event.kind, EventKind::TurnRequested { .. }))
        .expect("the request");
    assert_eq!(rerun.fork, parent[requested - 1].id());
    assert_eq!(rerun.depth, parent[requested - 1].depth + 1);
}

#[test]
fn a_boundary_snaps_the_fork_point_back_to_a_segment_start() {
    let scratch = Scratch::new();
    seed(&scratch);
    let mut reactor = reactor_with_a_model(&scratch.path, answer());
    // The turn and its request, so the tip is a model response with no calls and
    // is therefore not a segment start.
    reactor.step(now()).expect("writes the turn");
    reactor.step(now()).expect("sends");
    let parent = events(&scratch);
    let tip = parent.last().expect("a tip");
    assert!(matches!(tip.kind, EventKind::ModelResponded { .. }));

    let plain = reactor.fork(&fork_at(Selector::Tip), now()).expect("forks");
    assert_eq!(plain.depth, tip.depth + 1);

    let mut request = fork_at(Selector::Tip);
    request.boundary = true;
    let snapped = reactor.fork(&request, now()).expect("snaps");
    let requested = parent
        .iter()
        .find(|event| matches!(event.kind, EventKind::TurnRequested { .. }))
        .expect("the request");
    assert_eq!(snapped.fork, requested.id());
    assert_eq!(snapped.depth, requested.depth + 1);
    assert!(snapped.depth < plain.depth);
}

#[test]
fn a_fork_over_an_open_call_is_refused_unless_forced() {
    let scratch = Scratch::new();
    seed(&scratch);
    let mut store = Store::open(&scratch.path).expect("opens");
    let tip = store
        .load(ROOT_BRANCH)
        .expect("loads")
        .last()
        .expect("a tip")
        .clone();
    let intent = Event {
        branch: ROOT_BRANCH,
        depth: tip.depth + 1,
        prev: Some(tip.id()),
        time: now(),
        actor: Actor::Agent("ana".to_string()),
        causation: None,
        correlation: None,
        model: None,
        kind: EventKind::ToolIntent {
            args: "{}".to_string(),
            call: call(),
            env: Vec::new(),
            key: "k".to_string(),
            tool: "shell".to_string(),
        },
    };
    store.append(&intent).expect("appends");
    drop(store);

    let mut reactor = Reactor::open(&scratch.path).expect("opens");
    let before = total_events(&scratch);
    let error = reactor
        .fork(&fork_at(Selector::Tip), now())
        .expect_err("the hazard is refused");
    match error {
        ReactorError::ForkStranded { call: named, tool } => {
            assert_eq!(named, call());
            assert_eq!(tool, "shell");
        },
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(total_events(&scratch), before, "a refusal appends nothing");

    let mut force_request = fork_at(Selector::Tip);
    force_request.force = true;
    let forked = reactor.fork(&force_request, now()).expect("forced");
    assert_eq!(forked.fork, intent.id());
    assert_eq!(forked.depth, intent.depth + 1);
}

#[test]
fn a_fork_of_a_branch_that_is_not_there_is_refused() {
    let scratch = Scratch::new();
    let mut reactor = started(&scratch);
    let missing = BranchId::from_bytes([9; 8]);
    let mut request = fork_at(Selector::Tip);
    request.parent = missing;
    let error = reactor.fork(&request, now()).expect_err("no such parent");
    match error {
        ReactorError::NoSuchBranch(branch) => assert_eq!(branch, missing),
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn a_command_on_a_branch_that_is_not_there_is_refused_and_the_reactor_survives() {
    let scratch = Scratch::new();
    let mut reactor = started(&scratch);
    let missing = BranchId::from_bytes([9; 8]);
    let error = reactor
        .command(missing, "send", "hello", now())
        .expect_err("no such branch");
    match error {
        ReactorError::NoSuchBranch(branch) => assert_eq!(branch, missing),
        other => panic!("unexpected {other:?}"),
    }
    let command = crate::bus::Command::Send {
        branch: missing,
        text: "hello".to_string(),
    };
    let refusal = crate::route::route(&mut reactor, &command, now()).expect_err("usage");
    match refusal {
        crate::bus::Refusal::Usage(message) => assert!(message.contains("no branch")),
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(
        total_events(&scratch),
        2,
        "the genesis and the start, and a refusal appends nothing"
    );
}

#[test]
fn a_fork_that_names_no_event_turn_or_genesis_is_refused() {
    let scratch = Scratch::new();
    let (mut reactor, _, turn) = seeded_turn(&scratch);
    let before = total_events(&scratch);

    let unknown_event = EventId::from_bytes([9; 32]);
    let error = reactor
        .fork(&fork_at(Selector::At(unknown_event)), now())
        .expect_err("no such event");
    assert!(matches!(
        error,
        ReactorError::Fork(ForkError::UnknownEvent(id)) if id == unknown_event
    ));

    let unknown_turn = TurnId::from_bytes([9; 8]);
    assert_ne!(unknown_turn, turn);
    let error = reactor
        .fork(&fork_at(Selector::Turn(unknown_turn)), now())
        .expect_err("no such turn");
    assert!(matches!(
        error,
        ReactorError::Fork(ForkError::UnknownTurn(id)) if id == unknown_turn
    ));

    let error = reactor
        .fork(&fork_at(Selector::Rewind(u32::MAX)), now())
        .expect_err("before the genesis");
    assert!(matches!(
        error,
        ReactorError::Fork(ForkError::BeforeGenesis)
    ));
    assert_eq!(total_events(&scratch), before, "a refusal appends nothing");
}

#[test]
fn two_forks_of_the_same_point_are_two_branches() {
    let scratch = Scratch::new();
    let (mut reactor, _, _) = seeded_turn(&scratch);
    let first = reactor.fork(&fork_at(Selector::Tip), now()).expect("first");
    let second = reactor
        .fork(&fork_at(Selector::Tip), now())
        .expect("second");
    assert_eq!(first.fork, second.fork, "the same event, the same instant");
    assert_ne!(first.branch, second.branch);
    assert_ne!(first.event, second.event);

    let store = Store::open(&scratch.path).expect("opens");
    assert_eq!(store.load(first.branch).expect("loads").len(), 1);
    assert_eq!(store.load(second.branch).expect("loads").len(), 1);
    store.verify().expect("verifies");
}

#[test]
fn a_branchs_path_includes_the_event_at_the_cut_and_nothing_past_it() {
    let scratch = Scratch::new();
    let (mut reactor, parent, _) = seeded_turn(&scratch);
    let cut = parent[3].clone();
    let forked = reactor
        .fork(&fork_at(Selector::At(cut.id())), now())
        .expect("forks");

    let path = reactor.path(forked.branch).expect("reads the path");
    assert_eq!(
        path.len(),
        5,
        "four events through the cut and the creation"
    );
    assert!(
        path.iter().any(|event| event.id() == cut.id()),
        "the cut event is part of the path"
    );
    assert!(
        !path.iter().any(|event| event.id() == parent[4].id()),
        "an event past the cut is not"
    );
    assert_eq!(path.last().expect("the tip").prev, Some(cut.id()));
}

#[test]
fn a_fork_of_a_child_reads_the_whole_path() {
    let scratch = Scratch::new();
    let (mut reactor, parent, _) = seeded_turn(&scratch);
    let first = reactor.fork(&fork_at(Selector::Tip), now()).expect("first");
    let mut store = Store::open(&scratch.path).expect("opens");
    let child_message = append_message(&mut store, first.branch, "the child said");
    drop(store);

    let mut request = fork_at(Selector::Tip);
    request.parent = first.branch;
    let second = reactor.fork(&request, now()).expect("second");

    let path = reactor.path(second.branch).expect("reads the path");
    assert_eq!(path.len(), parent.len() + 3, "parent, child, and creation");
    assert_eq!(
        path.last().expect("the tip").prev,
        Some(child_message),
        "the second fork anchors to the child's own tip"
    );
    for event in &parent {
        assert!(
            path.iter().any(|candidate| candidate.id() == event.id()),
            "the parent's whole path is inherited"
        );
    }
}

#[test]
fn a_missing_branch_row_is_a_fold_error() {
    let scratch = Scratch::new();
    let (mut reactor, _, _) = seeded_turn(&scratch);
    let forked = reactor.fork(&fork_at(Selector::Tip), now()).expect("forks");
    drop(reactor);

    // Deleting a row is the one thing the store refuses, so the corruption is
    // made with a raw connection and the trigger out of the way.
    let raw = rusqlite::Connection::open(&scratch.path).expect("opens raw");
    raw.execute_batch("DROP TRIGGER events_no_delete;")
        .expect("drops the guard");
    raw.execute(
        "DELETE FROM events WHERE branch = ?1",
        [forked.branch.to_string()],
    )
    .expect("deletes the branch");
    drop(raw);

    let reactor = Reactor::open(&scratch.path).expect("opens");
    let error = reactor.path(forked.branch).expect_err("no rows");
    match error {
        ReactorError::Fold(FoldError::UnknownBranch(branch)) => {
            assert_eq!(branch, forked.branch);
        },
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn a_branch_whose_first_row_is_not_its_creation_is_a_fold_error() {
    let scratch = Scratch::new();
    seed(&scratch);
    let mut store = Store::open(&scratch.path).expect("opens");
    let tip = store
        .load(ROOT_BRANCH)
        .expect("loads")
        .last()
        .expect("a tip")
        .clone();
    let orphan = BranchId::from_bytes([9; 8]);
    let event = Event {
        branch: orphan,
        depth: tip.depth + 1,
        prev: Some(tip.id()),
        time: now(),
        actor: Actor::Process("test".to_string()),
        causation: None,
        correlation: None,
        model: None,
        kind: EventKind::InstanceStopping {
            reason: "test".to_string(),
        },
    };
    store.append(&event).expect("appends");
    drop(store);

    let reactor = Reactor::open(&scratch.path).expect("opens");
    let error = reactor.path(orphan).expect_err("no branch.created");
    match error {
        ReactorError::Fold(FoldError::UnknownBranch(branch)) => assert_eq!(branch, orphan),
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn a_parent_cycle_is_a_fold_error_instead_of_a_stack_overflow() {
    let scratch = Scratch::new();
    drop(started(&scratch));
    let mut store = Store::open(&scratch.path).expect("opens");
    let tip = store
        .load(ROOT_BRANCH)
        .expect("loads")
        .last()
        .expect("a tip")
        .clone();
    let cyclic = BranchId::from_bytes([9; 8]);
    let event = Event {
        branch: cyclic,
        depth: tip.depth + 1,
        prev: Some(tip.id()),
        time: now(),
        actor: Actor::Process("test".to_string()),
        causation: None,
        correlation: None,
        model: None,
        kind: EventKind::BranchCreated {
            parent: Some(cyclic),
            fork: Some(tip.id()),
            label: None,
        },
    };
    store.append(&event).expect("appends");
    drop(store);

    let reactor = Reactor::open(&scratch.path).expect("opens");
    let error = reactor.path(cyclic).expect_err("a cycle");
    match error {
        ReactorError::Fold(FoldError::UnknownBranch(branch)) => assert_eq!(branch, cyclic),
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn a_fork_over_the_socket_names_the_branch_it_appended() {
    let scratch = Scratch::new();
    let socket = scratch.path.with_extension("sock");
    let bus = crate::bus::Bus::bind(&socket).expect("binds");
    let mut reactor = started(&scratch);

    let target = socket;
    let line = format!(r#"{{"command":"fork","parent":"{ROOT_BRANCH}"}}"#);
    let client = std::thread::spawn(move || {
        use std::io::{BufRead as _, BufReader, Write as _};
        let mut stream = std::os::unix::net::UnixStream::connect(&target).expect("connects");
        stream.write_all(line.as_bytes()).expect("writes");
        stream.write_all(b"\n").expect("writes");
        stream.flush().expect("flushes");
        let mut reader = BufReader::new(stream);
        let mut answer = String::new();
        reader.read_line(&mut answer).expect("reads");
        answer.trim_end().to_string()
    });

    let started = std::time::Instant::now();
    let pending = loop {
        let mut pending = bus.poll();
        if let Some(pending) = pending.pop() {
            break pending;
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "the loop never polled the socket"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    };
    let command = pending.command().expect("a command").clone();
    let reply = route(&mut reactor, &command, now());
    pending.answer(&reply);

    let answer = client.join().expect("the client finished");
    let value: serde_json::Value = serde_json::from_str(&answer).expect("the reply is JSON");
    assert_eq!(value["ok"], true);
    let branch: BranchId = value["branch"]
        .as_str()
        .expect("the reply names the branch")
        .parse()
        .expect("a branch id");
    let depth = value["depth"].as_u64().expect("the reply names the depth");
    assert_eq!(depth, 2);
    let store = Store::open(&scratch.path).expect("opens");
    let child = store.load(branch).expect("loads");
    assert_eq!(child.len(), 1, "the branch the reply named was appended");
    assert_eq!(child[0].depth, depth);
}

#[test]
fn a_childs_first_request_starts_with_the_parents_transcript() {
    let scratch = Scratch::new();
    let (mut reactor, _, _) = seeded_turn(&scratch);
    let forked = reactor.fork(&fork_at(Selector::Tip), now()).expect("forks");
    drop(reactor);

    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut child = Reactor::with_model(
        &scratch.path,
        Box::new(Capturing {
            seen: Arc::clone(&seen),
        }),
        settings(),
    )
    .expect("opens");
    let parent: Vec<String> = child
        .state()
        .expect("folds")
        .transcript(ROOT_BRANCH)
        .iter()
        .map(|message| message.content.clone())
        .collect();
    assert!(!parent.is_empty(), "the fixture has a parent transcript");

    child
        .command(forked.branch, "send", "next", now())
        .expect("takes the send");
    child.step(now()).expect("writes the turn");
    child.step(now()).expect("sends");

    let seen = seen.lock().expect("the capture lock");
    assert_eq!(seen.len(), 1, "the first child request was captured");
    let contents: Vec<String> = seen[0]
        .messages
        .iter()
        .map(|message| message.content.clone())
        .collect();
    drop(seen);
    assert_eq!(&contents[..parent.len()], parent.as_slice());
    assert_eq!(
        contents.last().map(String::as_str),
        Some("next"),
        "then the child's own turn"
    );
}

#[test]
fn route_takes_a_send_to_the_branch_it_names() {
    let scratch = Scratch::new();
    let mut reactor = started(&scratch);
    let before = events(&scratch).len();
    let reply = route(
        &mut reactor,
        &Command::Send {
            branch: ROOT_BRANCH,
            text: "hello".to_string(),
        },
        now(),
    )
    .expect("takes the send");
    assert_eq!(reply, Reply::Taken);
    let written = events(&scratch);
    assert_eq!(written.len(), before + 1);
    match &written.last().expect("an event").kind {
        EventKind::CommandReceived {
            command,
            surface,
            text,
            ..
        } => {
            assert_eq!(command, "send");
            assert_eq!(surface, "bus");
            assert_eq!(text.as_deref(), Some("hello"));
        },
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn route_takes_a_yes_and_a_no_by_name() {
    let scratch = Scratch::new();
    let mut reactor = started(&scratch);
    let id = "0404040404040404:3".to_string();
    for (command, branch, expected) in [
        (
            Command::Approve {
                branch: ROOT_BRANCH,
                id: id.clone(),
            },
            ROOT_BRANCH,
            "approve",
        ),
        (
            Command::Deny {
                branch: ROOT_BRANCH,
                id: id.clone(),
            },
            ROOT_BRANCH,
            "deny",
        ),
    ] {
        let reply = route(&mut reactor, &command, now()).expect("takes the answer");
        assert_eq!(reply, Reply::Taken);
        let written = events(&scratch);
        let last = written.last().expect("an event");
        assert_eq!(last.branch, branch);
        match &last.kind {
            EventKind::CommandReceived {
                command: named,
                text,
                ..
            } => {
                assert_eq!(named, expected);
                assert_eq!(text.as_deref(), Some(id.as_str()));
            },
            other => panic!("unexpected {other:?}"),
        }
    }
}

#[test]
fn route_forks_and_reports_the_branch_and_depth() {
    let scratch = Scratch::new();
    let mut reactor = started(&scratch);
    let reply = route(&mut reactor, &Command::Fork(fork_at(Selector::Tip)), now()).expect("forks");
    let Reply::Forked { branch, depth } = reply else {
        panic!("a fork is not {reply:?}");
    };
    assert_eq!(depth, 2, "the root's started event is at depth one");
    let store = Store::open(&scratch.path).expect("opens");
    let child = store.load(branch).expect("loads");
    assert_eq!(child.len(), 1);
    assert_eq!(child[0].branch, branch);
    assert_eq!(child[0].depth, depth);
}

#[test]
fn route_maps_a_refused_fork_to_its_class() {
    let scratch = Scratch::new();
    let mut reactor = started(&scratch);
    let before = total_events(&scratch);
    let mut request = fork_at(Selector::Tip);
    request.parent = BranchId::from_bytes([9; 8]);
    let reply = route(&mut reactor, &Command::Fork(request), now());
    assert!(matches!(reply, Err(Refusal::Usage(_))), "{reply:?}");
    assert_eq!(total_events(&scratch), before);
}
