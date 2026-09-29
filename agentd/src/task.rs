//! The model-driven capability: run a task by driving a model and its tools.
//!
//! This is the second implementation of the [`Capability`] seam, and it is where
//! the loop nests. The loop in [`crate::loopcore`] turns one command into one
//! result; a task needs many model turns to get there, so this capability owns
//! the inner loop: ask the model, run whatever tools it asks for, feed the
//! results back, and stop when it answers with no tool call.
//!
//! The model is a [`Model`], so the whole capability is exercisable without a
//! network. `shell` is the one tool it offers today, which is why a task's
//! commands inherit the session's confinement and the same bounded output the
//! `shell` action uses. Nothing here spawns a process itself.
//!
//! Everything that crosses into the model's context is bounded, for the reason
//! [`crate::shell`] bounds a command's output: a model's context is a resource
//! like any other, and a command that writes without end must not be able to
//! fill it.

use serde_json::{Value, json};

use crate::contract::{Command, Output, OutputKind};
use crate::loopcore::{Capability, Reporter};
use crate::model::{Message, Model, Tool, ToolCall};
use crate::shell::{self, Shell};

/// The action this capability answers.
pub const ACTION: &str = "task";

/// The most model turns one task may take before it is stopped.
pub const MAX_TURNS: usize = 32;

/// How much of one of a tool result's streams is fed back to the model.
const MAX_TOOL_RESULT_BYTES: usize = 8 * 1024;

/// How much of a whole tool result is fed back to the model.
///
/// Twice the per-stream cap, plus room for the fields around the two streams, so
/// the bound holds even for a tool whose detail has fields this does not know.
const MAX_TOOL_MESSAGE_BYTES: usize = 2 * MAX_TOOL_RESULT_BYTES + 1024;

/// How much of the model's prose one progress report carries.
const MAX_REPORT_BYTES: usize = 2 * 1024;

/// What the agent tells the model about itself.
const SYSTEM_PROMPT: &str = "You are the agent for one session, working in that session's workspace. \
You have one tool, `shell`, which runs a program with arguments in the workspace and returns its exit \
code, its standard output, and its standard error. Use it to inspect and change files. When the task is \
complete, reply with your answer and no tool call.";

/// Run the `task` action: `detail` is `{"task": "..."}`.
#[derive(Debug)]
pub struct Task {
    /// The model the task is driven by.
    model: Box<dyn Model>,
    /// How the model's commands are run, in the session's workspace.
    shell: Shell,
    /// The most model turns one task may take.
    max_turns: usize,
}

impl Task {
    /// A task capability driven by `model`, running its commands through `shell`,
    /// which names the workspace they run in.
    #[must_use]
    pub fn new(
        model: Box<dyn Model>,
        shell: Shell,
    ) -> Self {
        Self {
            model,
            shell,
            max_turns: MAX_TURNS,
        }
    }

    /// A task capability with an explicit turn bound, for a caller that wants one.
    #[must_use]
    pub const fn with_max_turns(
        mut self,
        max_turns: usize,
    ) -> Self {
        self.max_turns = max_turns;
        self
    }

    /// Run one tool call.
    ///
    /// A name the agent does not offer is reported rather than refused, so the
    /// model is told what happened and gets to correct itself instead of the task
    /// ending.
    fn run_tool(
        &self,
        call: &ToolCall,
        reporter: &dyn Reporter,
    ) -> Output {
        if call.name != shell::ACTION {
            return Output {
                kind: OutputKind::Error,
                action: call.name.clone(),
                detail: json!({"reason": "the agent has no tool by that name"}),
            };
        }
        let command = Command {
            action: shell::ACTION.to_owned(),
            detail: json!({"argv": call.arguments.get("argv").cloned().unwrap_or(Value::Null)}),
        };
        self.shell.act(&command, reporter)
    }
}

impl Capability for Task {
    fn act(
        &self,
        command: &Command,
        reporter: &dyn Reporter,
    ) -> Output {
        let Some(task) = command.detail.get("task").and_then(Value::as_str) else {
            return failed(command, json!({"reason": "malformed task"}));
        };

        let tools = tools();
        let mut messages = vec![
            Message::System {
                content: SYSTEM_PROMPT.to_owned(),
            },
            Message::User {
                content: task.to_owned(),
            },
        ];

        for turn in 0..self.max_turns {
            let response = match self.model.complete(&messages, &tools) {
                Ok(response) => response,
                Err(error) => {
                    return failed(
                        command,
                        json!({
                            "reason": "the model call failed",
                            "error": error.to_string(),
                            "turn": turn,
                        }),
                    );
                },
            };

            // A turn with no tool call is the answer, so the task is done.
            if response.calls.is_empty() {
                return Output {
                    kind: OutputKind::Done,
                    action: command.action.clone(),
                    detail: json!({
                        "text": response.content,
                        "turns": turn + 1,
                    }),
                };
            }

            // What the model said while it worked, so a watcher follows the task
            // rather than only seeing it end.
            if !response.content.is_empty() {
                reporter.report(json!({
                    "turn": turn,
                    "text": bounded(&response.content, MAX_REPORT_BYTES),
                }));
            }

            // Run what it asked for, then feed each result back under the id of
            // the call that asked for it, which is how the next turn pairs them.
            let results: Vec<(String, String)> = response
                .calls
                .iter()
                .map(|call| (call.id.clone(), tool_result(&self.run_tool(call, reporter))))
                .collect();
            messages.push(Message::Assistant {
                content: response.content,
                calls: response.calls,
            });
            messages.extend(
                results
                    .into_iter()
                    .map(|(call_id, content)| Message::Tool { call_id, content }),
            );
        }

        failed(
            command,
            json!({"reason": "the task ran out of turns", "turns": self.max_turns}),
        )
    }
}

/// The tools the agent offers the model.
fn tools() -> Vec<Tool> {
    vec![Tool {
        name: shell::ACTION.to_owned(),
        description: "Run a program with arguments in the session workspace, and return its exit \
code, its standard output, and its standard error."
            .to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "argv": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "The program and its arguments, the program first.",
                },
            },
            "required": ["argv"],
            "additionalProperties": false,
        }),
    }]
}

/// The output for a task that could not finish.
fn failed(
    command: &Command,
    detail: Value,
) -> Output {
    Output {
        kind: OutputKind::Error,
        action: command.action.clone(),
        detail,
    }
}

/// What the model is told a tool returned.
///
/// The `shell` capability caps a command's output at a mebibyte, which is right
/// for the log and far too much for a model: a few turns of it would fill any
/// context window. Each stream is cut to [`MAX_TOOL_RESULT_BYTES`] first, so
/// neither stream can crowd the other out of the model's view, and the whole
/// result is then cut to [`MAX_TOOL_MESSAGE_BYTES`], so a tool whose detail this
/// does not know is bounded too.
fn tool_result(output: &Output) -> String {
    let mut detail = output.detail.clone();
    for field in ["stdout", "stderr"] {
        if let Some(Value::String(text)) = detail.get_mut(field) {
            let cut = bounded(text, MAX_TOOL_RESULT_BYTES);
            *text = cut;
        }
    }
    bounded(&detail.to_string(), MAX_TOOL_MESSAGE_BYTES)
}

/// `text` cut to at most `cap` bytes, marked when bytes were dropped.
///
/// The cut lands on a character boundary, so the result is valid UTF-8, and the
/// marker tells the model what it lost rather than letting it read a silently
/// shortened command's output as the whole of it.
fn bounded(
    text: &str,
    cap: usize,
) -> String {
    if text.len() <= cap {
        return text.to_owned();
    }
    let end = text.floor_char_boundary(cap);
    format!(
        "{}\n[truncated: {} bytes dropped]",
        &text[..end],
        text.len() - end
    )
}

#[cfg(test)]
mod tests {
    // Tests for the model-driven capability against a scripted model, so the
    // whole loop runs without a network. The one tool is real: a task that asks
    // for a command runs one.

    use std::path::PathBuf;
    use std::sync::Mutex;

    use super::*;
    use crate::model::{Error, Response};

    /// One request a [`Scripted`] model was given.
    #[derive(Debug, Clone)]
    struct Request {
        messages: Vec<Message>,
        tools: Vec<Tool>,
    }

    /// A model that answers from a script, so a task can be driven without one.
    ///
    /// The last answer repeats when the script runs out, which is how a test
    /// writes a model that never finishes.
    #[derive(Debug)]
    struct Scripted {
        answers: Mutex<Vec<Result<Response, String>>>,
        requests: Mutex<Vec<Request>>,
    }

    impl Scripted {
        fn new(answers: Vec<Result<Response, String>>) -> Self {
            assert!(!answers.is_empty(), "a script needs at least one answer");
            Self {
                answers: Mutex::new(answers),
                requests: Mutex::new(Vec::new()),
            }
        }

        /// The requests it was given, in order.
        fn requests(&self) -> Vec<Request> {
            self.requests.lock().expect("unpoisoned").clone()
        }
    }

    impl Model for Scripted {
        fn complete(
            &self,
            messages: &[Message],
            tools: &[Tool],
        ) -> Result<Response, Error> {
            self.requests.lock().expect("unpoisoned").push(Request {
                messages: messages.to_vec(),
                tools: tools.to_vec(),
            });
            let mut answers = self.answers.lock().expect("unpoisoned");
            let answer = match answers.len() {
                0 => unreachable!("the script is never emptied past its last answer"),
                1 => answers[0].clone(),
                _ => answers.remove(0),
            };
            answer.map_err(Error::Call)
        }
    }

    /// A reporter that records what it was told.
    #[derive(Debug, Default)]
    struct Reports(Mutex<Vec<Value>>);

    impl Reports {
        /// The details reported, in order.
        fn details(&self) -> Vec<Value> {
            self.0.lock().expect("unpoisoned").clone()
        }
    }

    impl Reporter for Reports {
        fn report(
            &self,
            detail: Value,
        ) {
            self.0.lock().expect("unpoisoned").push(detail);
        }
    }

    /// A temp workspace, removed on drop.
    struct Workspace(PathBuf);

    impl Workspace {
        fn new(tag: &str) -> Self {
            static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let unique = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("agent-task-{tag}-{}-{unique}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("creates the workspace");
            Self(path)
        }
    }

    impl Drop for Workspace {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// An assistant turn that only speaks.
    fn says(text: &str) -> Response {
        Response {
            content: text.to_owned(),
            calls: Vec::new(),
        }
    }

    /// An assistant turn that calls one tool.
    fn asks_for(
        name: &str,
        arguments: Value,
    ) -> Response {
        Response {
            content: String::new(),
            calls: vec![ToolCall {
                id: "call-1".to_owned(),
                name: name.to_owned(),
                arguments,
            }],
        }
    }

    /// A `task` command.
    fn task_action(task: &str) -> Command {
        Command {
            action: ACTION.to_owned(),
            detail: json!({"task": task}),
        }
    }

    /// A task capability over a fresh workspace, driven by `answers`.
    ///
    /// The shell keeps its own timeout, which every command here is well inside:
    /// each finishes on its own, so no test leans on the timeout.
    fn task(answers: Vec<Result<Response, String>>) -> (Task, std::sync::Arc<Scripted>, Workspace) {
        let workspace = Workspace::new("task");
        let model = std::sync::Arc::new(Scripted::new(answers));
        let shell = Shell::in_workspace(&workspace.0);
        (
            Task::new(Box::new(ArcModel(model.clone())), shell),
            model,
            workspace,
        )
    }

    /// Wraps an `Arc<Scripted>` as a `Model`, so a test can still read it.
    #[derive(Debug)]
    struct ArcModel(std::sync::Arc<Scripted>);

    impl Model for ArcModel {
        fn complete(
            &self,
            messages: &[Message],
            tools: &[Tool],
        ) -> Result<Response, Error> {
            self.0.complete(messages, tools)
        }
    }

    #[test]
    fn a_task_the_model_answers_without_a_tool_is_done() {
        let (task, model, _workspace) = task(vec![Ok(says("there is nothing to do"))]);
        let output = task.act(&task_action("tidy up"), &Reports::default());

        assert_eq!(output.kind, OutputKind::Done);
        assert_eq!(output.action, ACTION);
        assert_eq!(output.detail["text"], "there is nothing to do");
        assert_eq!(output.detail["turns"], 1, "one turn was enough");
        assert_eq!(model.requests().len(), 1);
    }

    #[test]
    fn the_model_is_told_what_it_is_and_what_the_work_is() {
        let (task, model, _workspace) = task(vec![Ok(says("ok"))]);
        task.act(&task_action("write a file"), &Reports::default());

        let requests = model.requests();
        let request = requests.first().expect("the model was asked once");

        match request.messages.first().expect("a system message") {
            Message::System { content } => assert!(
                content.contains("workspace"),
                "the model is told where it works: {content}"
            ),
            other => panic!("expected a system message, got {other:?}"),
        }
        match request.messages.get(1).expect("a user message") {
            Message::User { content } => assert_eq!(content, "write a file"),
            other => panic!("expected a user message, got {other:?}"),
        }
    }

    #[test]
    fn the_shell_tool_is_offered_with_a_schema_that_names_what_it_needs() {
        let (task, model, _workspace) = task(vec![Ok(says("ok"))]);
        task.act(&task_action("write a file"), &Reports::default());

        let requests = model.requests();
        let request = requests.first().expect("the model was asked once");
        assert_eq!(request.tools.len(), 1, "one tool is offered today");
        let tool = request.tools.first().expect("one tool");
        assert_eq!(tool.name, "shell");
        assert!(
            !tool.description.is_empty(),
            "the model decides from the description"
        );
        assert_eq!(
            tool.parameters["required"],
            json!(["argv"]),
            "the schema names what the tool needs"
        );
    }

    #[test]
    fn a_task_runs_the_tool_the_model_asks_for() {
        let (task, model, _workspace) = task(vec![
            Ok(asks_for(
                "shell",
                json!({"argv": ["/bin/sh", "-c", "echo hi"]}),
            )),
            Ok(says("it printed hi")),
        ]);
        let output = task.act(&task_action("print hi"), &Reports::default());

        assert_eq!(output.kind, OutputKind::Done);
        assert_eq!(output.detail["turns"], 2, "one turn to call, one to answer");

        // The second request carries the result, under the id of the call that
        // asked for it, which is how a provider pairs the two.
        let requests = model.requests();
        let second = requests.get(1).expect("the model was asked twice");
        match second.messages.last().expect("a tool message") {
            Message::Tool { call_id, content } => {
                assert_eq!(call_id, "call-1");
                assert!(content.contains("hi"), "the output is fed back: {content}");
            },
            other => panic!("expected a tool message, got {other:?}"),
        }
    }

    #[test]
    fn the_models_command_runs_in_the_session_workspace() {
        let (task, _model, workspace) = task(vec![
            Ok(asks_for(
                "shell",
                json!({"argv": ["/bin/sh", "-c", "echo x > made.txt"]}),
            )),
            Ok(says("done")),
        ]);
        task.act(&task_action("make a file"), &Reports::default());

        assert_eq!(
            std::fs::read_to_string(workspace.0.join("made.txt")).expect("the file is on the host"),
            "x\n",
            "the command ran in the session's workspace"
        );
    }

    #[test]
    fn what_the_model_says_while_it_works_is_reported() {
        // A turn can say something and still call a tool, and that is exactly the
        // narration a watcher wants while the task runs.
        let turn = Response {
            content: "let me look around".to_owned(),
            calls: vec![ToolCall {
                id: "call-1".to_owned(),
                name: "shell".to_owned(),
                arguments: json!({"argv": ["/bin/sh", "-c", "exit 0"]}),
            }],
        };
        let (task, _model, _workspace) = task(vec![Ok(turn), Ok(says("done"))]);
        let reports = Reports::default();
        task.act(&task_action("look around"), &reports);

        // The turn is reported first, and the command it asked for is reported by
        // `shell` itself, so the composition of the two shows up in one place.
        assert_eq!(
            reports.details(),
            vec![
                json!({"turn": 0, "text": "let me look around"}),
                json!({"argv": ["/bin/sh", "-c", "exit 0"]}),
            ],
            "the turn is reported, and so is the command it asked for"
        );
    }

    #[test]
    fn a_task_stops_when_the_model_never_answers() {
        // The script repeats its last answer, so the model never stops calling a
        // tool and the turn bound is what ends the task.
        let (task, model, _workspace) = task(vec![Ok(asks_for(
            "shell",
            json!({"argv": ["/bin/sh", "-c", "exit 0"]}),
        ))]);
        let task = task.with_max_turns(3);
        let output = task.act(&task_action("never finish"), &Reports::default());

        assert_eq!(output.kind, OutputKind::Error);
        assert_eq!(output.detail["reason"], "the task ran out of turns");
        assert_eq!(output.detail["turns"], 3);
        assert_eq!(
            model.requests().len(),
            3,
            "the bound stops the loop rather than the model"
        );
    }

    #[test]
    fn a_task_with_no_turns_left_does_not_ask_the_model() {
        // The bound is a bound: at zero the model is never called.
        let (task, model, _workspace) = task(vec![Ok(says("unreachable"))]);
        let output = task
            .with_max_turns(0)
            .act(&task_action("do nothing"), &Reports::default());

        assert_eq!(output.kind, OutputKind::Error);
        assert_eq!(output.detail["reason"], "the task ran out of turns");
        assert!(model.requests().is_empty(), "the model was never asked");
    }

    #[test]
    fn a_model_that_fails_is_reported() {
        let (task, _model, _workspace) = task(vec![Err("no route to host".to_owned())]);
        let output = task.act(&task_action("anything"), &Reports::default());

        assert_eq!(output.kind, OutputKind::Error);
        assert_eq!(output.detail["reason"], "the model call failed");
        assert_eq!(output.detail["turn"], 0, "it failed on the first turn");
        let error = output.detail["error"].as_str().expect("a string");
        assert!(
            error.contains("no route to host"),
            "the model's own reason survives: {error}"
        );
    }

    #[test]
    fn a_command_without_a_task_is_reported_not_run() {
        let (task, model, _workspace) = task(vec![Ok(says("unreachable"))]);
        let output = task.act(
            &Command {
                action: ACTION.to_owned(),
                detail: json!({}),
            },
            &Reports::default(),
        );

        assert_eq!(output.kind, OutputKind::Error);
        assert_eq!(output.detail["reason"], "malformed task");
        assert!(model.requests().is_empty(), "the model is never asked");
    }

    #[test]
    fn a_command_whose_task_is_not_a_string_is_reported_not_run() {
        let (task, model, _workspace) = task(vec![Ok(says("unreachable"))]);
        let output = task.act(
            &Command {
                action: ACTION.to_owned(),
                detail: json!({"task": 7}),
            },
            &Reports::default(),
        );

        assert_eq!(output.kind, OutputKind::Error);
        assert_eq!(output.detail["reason"], "malformed task");
        assert!(model.requests().is_empty(), "the model is never asked");
    }

    #[test]
    fn an_unknown_tool_is_reported_to_the_model_rather_than_ending_the_task() {
        // The model is told what happened and gets to correct itself.
        let (task, model, _workspace) = task(vec![
            Ok(asks_for("teleport", json!({}))),
            Ok(says("sorry, I will use the shell")),
        ]);
        let output = task.act(&task_action("teleport away"), &Reports::default());

        assert_eq!(output.kind, OutputKind::Done, "the task continued");
        let requests = model.requests();
        let second = requests.get(1).expect("the model was asked twice");
        match second.messages.last().expect("a tool message") {
            Message::Tool { content, .. } => assert!(
                content.contains("no tool by that name"),
                "the model is told the tool is unknown: {content}"
            ),
            other => panic!("expected a tool message, got {other:?}"),
        }
    }

    #[test]
    fn a_tool_result_is_bounded_before_the_model_sees_it() {
        // A command that writes without end is capped at a mebibyte for the log,
        // which would fill a context window in a turn or two. What crosses into
        // the model is bounded again, and marked.
        let (task, model, _workspace) = task(vec![
            Ok(asks_for(
                "shell",
                json!({"argv": [
                    "/bin/sh",
                    "-c",
                    "i=0; while [ $i -lt 5000 ]; do echo aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa; i=$((i+1)); done",
                ]}),
            )),
            Ok(says("done")),
        ]);
        task.act(&task_action("write a lot"), &Reports::default());

        let requests = model.requests();
        let second = requests.get(1).expect("the model was asked twice");
        match second.messages.last().expect("a tool message") {
            Message::Tool { content, .. } => {
                assert!(
                    content.len() <= MAX_TOOL_MESSAGE_BYTES,
                    "the result is bounded, was {} bytes",
                    content.len()
                );
                assert!(
                    content.contains("[truncated:"),
                    "the model is told it was cut: {}",
                    &content[..content.len().min(200)]
                );
            },
            other => panic!("expected a tool message, got {other:?}"),
        }
    }

    #[test]
    fn neither_stream_can_crowd_the_other_out_of_the_models_view() {
        // Bounding each stream separately is what makes this hold. Without it, one
        // stream of a hundred and sixty kilobytes takes the whole result's budget
        // and the other stream -- and the flags after them both -- are gone from
        // what the model is told.
        let (task, model, _workspace) = task(vec![
            Ok(asks_for(
                "shell",
                json!({"argv": [
                    "/bin/sh",
                    "-c",
                    "echo out-marker; \
                i=0; while [ $i -lt 5000 ]; do echo aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa; i=$((i+1)); done; \
                echo err-marker >&2; \
                j=0; while [ $j -lt 5000 ]; do echo bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb >&2; j=$((j+1)); done",
                ]}),
            )),
            Ok(says("done")),
        ]);
        task.act(&task_action("write a lot"), &Reports::default());

        let requests = model.requests();
        let second = requests.get(1).expect("the model was asked twice");
        let content = match second.messages.last().expect("a tool message") {
            Message::Tool { content, .. } => content.clone(),
            other => panic!("expected a tool message, got {other:?}"),
        };

        assert!(
            content.contains("out-marker"),
            "the standard output survives a huge standard error"
        );
        assert!(
            content.contains("err-marker"),
            "the standard error survives a huge standard output"
        );
        assert!(
            content.contains("\"truncated\":"),
            "the flags after both streams survive, so the result was cut per stream"
        );
        assert!(
            content.len() <= MAX_TOOL_MESSAGE_BYTES,
            "the result is bounded, was {} bytes",
            content.len()
        );
    }

    #[test]
    fn a_tool_that_fails_is_reported_to_the_model() {
        // A command the capability refuses, or one that cannot start, still has
        // to reach the model as a reason rather than as empty output.
        let (task, model, _workspace) = task(vec![
            Ok(asks_for("shell", json!({"argv": []}))),
            Ok(says("I will try another way")),
        ]);
        task.act(&task_action("run nothing"), &Reports::default());

        let requests = model.requests();
        let second = requests.get(1).expect("the model was asked twice");
        match second.messages.last().expect("a tool message") {
            Message::Tool { content, .. } => assert!(
                content.contains("malformed argv"),
                "the refusal reaches the model: {content}"
            ),
            other => panic!("expected a tool message, got {other:?}"),
        }
    }

    #[test]
    fn a_result_under_the_cap_is_not_marked() {
        let output = Output {
            kind: OutputKind::Done,
            action: "shell".to_owned(),
            detail: json!({"stdout": "hi\n", "stderr": "", "code": 0}),
        };
        let result = tool_result(&output);
        assert!(!result.contains("[truncated:"), "nothing was dropped");
        assert!(result.contains("hi"), "the output is there: {result}");
    }

    #[test]
    fn a_bound_is_not_shortened_when_it_fits_exactly() {
        // The other side of the bound: exactly the cap is not a truncation.
        assert_eq!(bounded("abc", 3), "abc");
        assert_eq!(bounded("", 0), "");
    }

    #[test]
    fn text_over_a_bound_is_cut_and_marked() {
        let cut = bounded("abcdef", 3);
        assert!(
            cut.starts_with("abc\n[truncated: 3 bytes dropped]"),
            "the head is kept and the loss is named: {cut}"
        );
    }

    #[test]
    fn a_cut_lands_on_a_character_boundary() {
        // `é` is two bytes, and the cap of two lands inside the first one. Cutting
        // there would panic on a slice and produce invalid UTF-8 otherwise, so the
        // cut has to back up to the boundary and say how much it really dropped.
        assert_eq!(
            bounded("aéé", 2),
            "a\n[truncated: 4 bytes dropped]",
            "the cut backs up to the boundary"
        );
    }

    #[test]
    fn a_turn_that_says_nothing_is_not_reported() {
        // A turn that only calls a tool has nothing to narrate, and an empty
        // report would be noise on the log.
        let (task, _model, _workspace) = task(vec![
            Ok(asks_for(
                "shell",
                json!({"argv": ["/bin/sh", "-c", "exit 0"]}),
            )),
            Ok(says("done")),
        ]);
        let reports = Reports::default();
        task.act(&task_action("say nothing"), &reports);

        assert_eq!(
            reports.details(),
            vec![json!({"argv": ["/bin/sh", "-c", "exit 0"]})],
            "only the command is reported, not an empty turn"
        );
    }

    #[test]
    fn the_bounds_are_pinned() {
        // The bounds are arithmetic, and a mutant that changes one silently
        // changes what a model may be told. Spell them out.
        assert_eq!(MAX_TOOL_RESULT_BYTES, 8_192);
        assert_eq!(MAX_TOOL_MESSAGE_BYTES, 17_408);
        assert_eq!(MAX_REPORT_BYTES, 2_048);
        assert_eq!(MAX_TURNS, 32);
    }
}
