use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use agent_core::event::{Event, EventKind, FinishReason};
use agent_core::ids::{Actor, BranchId, CallId, TurnId};
use agent_core::time::Timestamp;
use agent_model::{Completion, Delta, Profile, Provider, ProviderError, Usage};
use agent_store::Store;

use super::{Gate, ROOT_BRANCH, Reactor, Settings, Step, Tool, Work, draw_of, gate};
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
        tools: vec![Tool {
            risk: agent_core::event::Risk::Read,
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
        Err(ProviderError::Transport)
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
        risk: agent_core::event::Risk::Read,
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
    let marker = scratch.path.with_extension("ran");
    Tool {
        risk: agent_core::event::Risk::Read,
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
fn a_command_from_the_socket_becomes_an_event_the_loop_can_read() {
    let scratch = Scratch::new();
    let socket = scratch.path.with_extension("sock");
    let bus = crate::bus::Bus::bind(&socket).expect("binds");
    let mut reactor = Reactor::open(&scratch.path).expect("opens");
    reactor.start(now()).expect("starts");

    // The client is the shape `agentctl` writes, so this covers the wire as well
    // as the writer.
    let target = socket;
    let client = std::thread::spawn(move || {
        use std::io::{BufRead as _, BufReader, Write as _};
        let mut stream = std::os::unix::net::UnixStream::connect(&target).expect("connects");
        let line = crate::bus::request_line("send", "hello from the socket");
        stream.write_all(line.as_bytes()).expect("writes");
        stream.write_all(b"\n").expect("writes");
        stream.flush().expect("flushes");
        let mut reader = BufReader::new(stream);
        let mut answer = String::new();
        reader.read_line(&mut answer).expect("reads");
        answer
    });

    let started = std::time::Instant::now();
    let answers = loop {
        let answers = bus.poll();
        if !answers.is_empty() {
            break answers;
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "the loop never polled the socket"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    };
    let answer = client.join().expect("the client finished");
    assert!(answer.contains(r#""ok":true"#), "{answer}");

    let command = answers
        .into_iter()
        .next()
        .expect("one answer")
        .expect("a command");
    reactor
        .command(ROOT_BRANCH, &command, now())
        .expect("writes the command");
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
