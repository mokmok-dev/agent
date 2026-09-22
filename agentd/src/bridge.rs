//! The protocol-conversion layer between a supervised child process and the
//! event log.
//!
//! A third-party tool speaks its own protocol over stdio; it does not speak
//! `CloudEvents`. A [`Protocol`] is a factory shared across sessions; it creates
//! one stateful [`Bridge`] per supervised child. The bridge turns the child's
//! output lines and the events routed to it into [`Action`]s — events to append
//! or lines to write — and the session manager performs them. The manager never
//! inspects the protocol payload, so adding a protocol does not touch it.
//!
//! Two protocols ship: `mcp` ([`McpProtocol`]), the Model Context Protocol over
//! stdio, a line protocol with no real state; and `acp` ([`AcpProtocol`]), the
//! Agent Client Protocol, a client-side state machine that negotiates a session
//! and answers the agent's permission requests over the log.

use std::collections::HashMap;

use agentd_events::Event;
use serde_json::{Value, json};

/// The event type carrying one inbound protocol message, from a bridged child
/// to the log.
pub const PROTOCOL_INBOUND: &str = "session.protocol.inbound";
/// The event type carrying one outbound protocol message.
///
/// It is the log-to-child direction, and is distinct from [`PROTOCOL_INBOUND`]
/// so an inbound message is never routed straight back to the child that
/// produced it.
pub const PROTOCOL_OUTBOUND: &str = "session.protocol.outbound";
/// A client asks a bridged child to start a prompt turn.
pub const PROMPT_REQUESTED: &str = "session.prompt.requested";
/// The protocol handshake completed; the child accepts prompts.
pub const PROTOCOL_READY: &str = "session.protocol.ready";
/// The protocol handshake failed, so the child cannot be driven at all.
pub const PROTOCOL_FAILED: &str = "session.protocol.failed";
/// A prompt turn ended, carrying its stop reason.
pub const PROMPT_COMPLETED: &str = "session.prompt.completed";
/// A prompt turn failed: the child answered the prompt with an error.
pub const PROMPT_FAILED: &str = "session.prompt.failed";
/// A bridged child asks for permission to run a tool call.
pub const SESSION_PERMISSION_REQUESTED: &str = "session.permission.requested";
/// An approver allows the request, selecting one of the options the child
/// offered.
pub const SESSION_PERMISSION_GRANTED: &str = "session.permission.granted";
/// An approver refuses the request, selecting a rejecting option when the child
/// offered one.
pub const SESSION_PERMISSION_DENIED: &str = "session.permission.denied";
/// Nobody decided the request in time, so it was withdrawn.
pub const SESSION_PERMISSION_CANCELLED: &str = "session.permission.cancelled";
/// The three outcomes a request may be answered with.
///
/// Exactly one follows a [`SESSION_PERMISSION_REQUESTED`], so a consumer that
/// matches one type must match all three or it will silently ignore two thirds
/// of the answers.
pub const SESSION_PERMISSION_DECISIONS: [&str; 3] = [
    SESSION_PERMISSION_GRANTED,
    SESSION_PERMISSION_DENIED,
    SESSION_PERMISSION_CANCELLED,
];

/// Whether `r#type` is one of the [`SESSION_PERMISSION_DECISIONS`].
#[must_use]
pub fn is_permission_decision(r#type: &str) -> bool {
    SESSION_PERMISSION_DECISIONS.contains(&r#type)
}

/// A client asks a bridged agent to start a prompt turn with `blocks` (ACP
/// content blocks, e.g. `[{"type":"text","text":"hi"}]`).
#[must_use]
pub fn prompt_requested(blocks: &Value) -> Event {
    Event::new(
        PROMPT_REQUESTED,
        json!({ "protocol": "acp", "prompt": blocks }),
    )
}

/// An approver allows the tool call `request_id` with `option_id`, the id of
/// one of the options the child offered.
#[must_use]
pub fn permission_granted(
    request_id: &str,
    option_id: &str,
) -> Event {
    Event::new(
        SESSION_PERMISSION_GRANTED,
        json!({
            "request_id": request_id,
            "decision": "granted",
            "option_id": option_id,
        }),
    )
}

/// An approver refuses the tool call `request_id`, selecting the rejecting
/// `option_id` the child offered.
#[must_use]
pub fn permission_denied(
    request_id: &str,
    option_id: &str,
) -> Event {
    Event::new(
        SESSION_PERMISSION_DENIED,
        json!({
            "request_id": request_id,
            "decision": "denied",
            "option_id": option_id,
        }),
    )
}

/// Nobody decided the tool call `request_id`, so the request is withdrawn.
#[must_use]
pub fn permission_cancelled(request_id: &str) -> Event {
    Event::new(
        SESSION_PERMISSION_CANCELLED,
        json!({ "request_id": request_id, "decision": "cancelled" }),
    )
}

/// The prefix of a bridged event's `subject`, addressing one session's child.
pub const SUBJECT_PREFIX: &str = "session:";

/// The `subject` attribute that addresses `session_id`'s child.
#[must_use]
pub fn session_subject(session_id: &str) -> String {
    format!("{SUBJECT_PREFIX}{session_id}")
}

/// The JSON-RPC version every bridged message declares.
const JSONRPC_VERSION: &str = "2.0";

/// One step a bridge asks the manager to take.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Append this event (the manager stamps `subject`).
    Publish(Event),
    /// Write this line to the child.
    Write(String),
}

/// What a protocol needs to know about the session it is connecting.
///
/// `connect` returns a `'static` bridge, so an implementor must copy anything
/// it keeps out of the borrowed context.
#[derive(Debug, Clone, Copy)]
pub struct SessionContext<'a> {
    /// The daemon's session id, used in the `subject` of emitted events.
    pub session_id: &'a str,
    /// The sandbox working directory, which an agent advertises as the ACP
    /// `cwd` and works inside.
    pub workdir: &'a std::path::Path,
}

/// A protocol, shared across sessions. It creates one state machine per child.
pub trait Protocol: Send + Sync {
    /// A fresh state machine for one session.
    fn connect(
        &self,
        context: &SessionContext<'_>,
    ) -> Box<dyn Bridge>;
}

/// The per-session conversion state machine.
///
/// The methods are synchronous and take `&mut self`: a bridge owns its phase
/// and id table for one child, and all I/O stays in the manager.
pub trait Bridge: Send {
    /// The opening exchange, called once when the child starts.
    fn start(&mut self) -> Vec<Action>;

    /// Converts one event routed to this session into actions.
    fn on_event(
        &mut self,
        event: &Event,
    ) -> Vec<Action>;

    /// Converts one line of the child's output into actions.
    fn on_line(
        &mut self,
        line: &str,
    ) -> Vec<Action>;
}

/// Builds the event for one inbound protocol message.
///
/// The `subject` attribute is deliberately not set here: the manager that knows
/// the session attaches it, so a bridge cannot misattribute a message.
#[must_use]
pub fn protocol_inbound(
    protocol: &str,
    message: &Value,
) -> Event {
    Event::new(
        PROTOCOL_INBOUND,
        json!({ "protocol": protocol, "message": message }),
    )
}

/// Parses one line as a JSON-RPC 2.0 message, or `None`.
fn parse_message(line: &str) -> Option<Value> {
    let message: Value = serde_json::from_str(line).ok()?;
    (message.get("jsonrpc").and_then(Value::as_str) == Some(JSONRPC_VERSION)).then_some(message)
}

/// The Model Context Protocol over stdio: newline-delimited JSON-RPC 2.0.
#[derive(Debug, Clone)]
pub struct McpProtocol {
    protocol_version: String,
    client_name: String,
}

impl McpProtocol {
    /// Creates a protocol that announces `client_name` at
    /// [`DEFAULT_PROTOCOL_VERSION`](Self::DEFAULT_PROTOCOL_VERSION).
    #[must_use]
    pub fn new(client_name: impl Into<String>) -> Self {
        Self {
            protocol_version: Self::DEFAULT_PROTOCOL_VERSION.to_string(),
            client_name: client_name.into(),
        }
    }

    /// Overrides the protocol revision announced in the handshake.
    #[must_use]
    pub fn with_protocol_version(
        mut self,
        protocol_version: impl Into<String>,
    ) -> Self {
        self.protocol_version = protocol_version.into();
        self
    }

    /// The MCP revision this protocol announces by default.
    pub const DEFAULT_PROTOCOL_VERSION: &'static str = "2024-11-05";
}

impl Default for McpProtocol {
    fn default() -> Self {
        Self::new("agentd")
    }
}

impl Protocol for McpProtocol {
    fn connect(
        &self,
        _context: &SessionContext<'_>,
    ) -> Box<dyn Bridge> {
        Box::new(McpBridge {
            protocol_version: self.protocol_version.clone(),
            client_name: self.client_name.clone(),
        })
    }
}

/// The MCP state machine. MCP is a request/response codec with no phase, so the
/// machine's only state is the configured handshake.
struct McpBridge {
    protocol_version: String,
    client_name: String,
}

impl McpBridge {
    /// Builds the `initialize` request, the first message of an MCP session.
    fn initialize_request(&self) -> String {
        json!({
            "jsonrpc": JSONRPC_VERSION,
            "id": 0,
            "method": "initialize",
            "params": {
                "protocolVersion": self.protocol_version,
                "capabilities": {},
                "clientInfo": {
                    "name": self.client_name,
                    "version": env!("CARGO_PKG_VERSION"),
                },
            },
        })
        .to_string()
    }

    /// Whether `message` is the response to the bridge's `initialize` request,
    /// which an MCP server expects to be followed by `notifications/initialized`.
    fn is_initialize_response(message: &Value) -> bool {
        message.get("id").and_then(Value::as_u64) == Some(0)
            && (message.get("result").is_some() || message.get("error").is_some())
    }
}

impl Bridge for McpBridge {
    fn start(&mut self) -> Vec<Action> {
        vec![Action::Write(self.initialize_request())]
    }

    fn on_event(
        &mut self,
        event: &Event,
    ) -> Vec<Action> {
        if event.r#type != PROTOCOL_OUTBOUND
            || event.data.get("protocol").and_then(Value::as_str) != Some("mcp")
        {
            return Vec::new();
        }
        let Some(message) = event.data.get("message") else {
            return Vec::new();
        };
        if message.get("jsonrpc").and_then(Value::as_str) != Some(JSONRPC_VERSION) {
            return Vec::new();
        }
        vec![Action::Write(message.to_string())]
    }

    fn on_line(
        &mut self,
        line: &str,
    ) -> Vec<Action> {
        let Some(message) = parse_message(line) else {
            return Vec::new();
        };
        let mut actions = vec![Action::Publish(protocol_inbound("mcp", &message))];
        if Self::is_initialize_response(&message) {
            actions.push(Action::Write(
                json!({ "jsonrpc": JSONRPC_VERSION, "method": "notifications/initialized" })
                    .to_string(),
            ));
        }
        actions
    }
}

/// The Agent Client Protocol: a client-side state machine over stdio JSON-RPC.
#[derive(Debug, Clone)]
pub struct AcpProtocol {
    client_name: String,
    protocol_version: u16,
}

impl AcpProtocol {
    /// Creates a protocol that announces `client_name`.
    #[must_use]
    pub fn new(client_name: impl Into<String>) -> Self {
        Self {
            client_name: client_name.into(),
            protocol_version: Self::DEFAULT_PROTOCOL_VERSION,
        }
    }

    /// The ACP major version this client announces by default.
    pub const DEFAULT_PROTOCOL_VERSION: u16 = 1;

    /// Overrides the ACP major version announced in `initialize`.
    #[must_use]
    pub const fn with_protocol_version(
        mut self,
        protocol_version: u16,
    ) -> Self {
        self.protocol_version = protocol_version;
        self
    }
}

impl Default for AcpProtocol {
    fn default() -> Self {
        Self::new("agentd")
    }
}

impl Protocol for AcpProtocol {
    fn connect(
        &self,
        context: &SessionContext<'_>,
    ) -> Box<dyn Bridge> {
        Box::new(AcpBridge {
            session_id: context.session_id.to_string(),
            workdir: context.workdir.to_path_buf(),
            client_name: self.client_name.clone(),
            protocol_version: self.protocol_version,
            phase: Phase::Started,
            next_id: 0,
            acp_session_id: None,
            pending: HashMap::new(),
            inbound: HashMap::new(),
        })
    }
}

/// How far the client side of the ACP handshake has progressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// `initialize` sent, its response awaited.
    Started,
    /// `initialize` answered, `session/new` sent.
    Initialized,
    /// A session exists; prompts may be sent.
    Ready,
    /// A prompt turn is in flight.
    Prompting,
    /// The handshake was rejected; the child cannot be driven.
    Failed,
}

/// What an outbound JSON-RPC id was allocated for, so its response advances the
/// right state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pending {
    Initialize,
    NewSession,
    Prompt,
}

/// The ACP client state machine for one supervised agent.
struct AcpBridge {
    /// The daemon's session id, used in the `subject` of the events it emits.
    session_id: String,
    /// The sandbox working directory, advertised as the ACP `cwd`.
    workdir: std::path::PathBuf,
    client_name: String,
    protocol_version: u16,
    phase: Phase,
    /// The next outbound JSON-RPC id.
    next_id: u64,
    /// The ACP session id from `session/new`, needed by `session/prompt`.
    acp_session_id: Option<String>,
    /// Outbound requests awaiting a response, keyed by JSON-RPC id.
    pending: HashMap<u64, Pending>,
    /// Inbound requests from the agent awaiting a decision, keyed by the
    /// JSON-RPC id's string form, so a decision can be answered under the same
    /// id.
    inbound: HashMap<String, Value>,
}

impl AcpBridge {
    /// Allocates the next outbound JSON-RPC id for `pending`.
    fn request(
        &mut self,
        pending: Pending,
        method: &str,
        params: &Value,
    ) -> Action {
        let id = self.next_id;
        self.next_id += 1;
        self.pending.insert(id, pending);
        Action::Write(
            json!({
                "jsonrpc": JSONRPC_VERSION,
                "id": id,
                "method": method,
                "params": params,
            })
            .to_string(),
        )
    }

    /// The client capabilities advertised in `initialize`. The agent does its
    /// own filesystem and shell work inside the sandbox, so the daemon proxies
    /// none of it.
    fn client_capabilities() -> Value {
        json!({
            "fs": { "readTextFile": false, "writeTextFile": false },
            "terminal": false,
        })
    }

    /// Handles an inbound request (one with a `method` and an `id`).
    fn on_request(
        &mut self,
        id: &Value,
        method: &str,
        params: &Value,
    ) -> Vec<Action> {
        let message =
            json!({ "jsonrpc": JSONRPC_VERSION, "id": id, "method": method, "params": params });
        let mut actions = vec![Action::Publish(protocol_inbound("acp", &message))];
        if method == "session/request_permission" {
            let request_id = id_to_string(id);
            self.inbound.insert(request_id.clone(), id.clone());
            actions.push(Action::Publish(Event::new(
                SESSION_PERMISSION_REQUESTED,
                json!({
                    "protocol": "acp",
                    "session_id": self.session_id,
                    "request_id": request_id,
                    "tool_call": params.get("toolCall").cloned().unwrap_or(Value::Null),
                    "options": params.get("options").cloned().unwrap_or(Value::Null),
                }),
            )));
        } else {
            // The daemon advertises no other client capability, so any other
            // request is answered with JSON-RPC's `Method not found` (code
            // -32601) rather than left hanging on the agent's pending id.
            actions.push(Action::Write(
                json!({
                    "jsonrpc": JSONRPC_VERSION,
                    "id": id,
                    "error": { "code": -32601, "message": format!("method not supported: {method}") },
                })
                .to_string(),
            ));
        }
        actions
    }

    /// Handles an inbound response (one with an `id` and `result`/`error`),
    /// advancing the handshake and publishing the message.
    fn on_response(
        &mut self,
        id: u64,
        message: &Value,
    ) -> Vec<Action> {
        let mut actions = vec![Action::Publish(protocol_inbound("acp", message))];
        match self.pending.remove(&id) {
            Some(Pending::Initialize) => {
                if message.get("error").is_some() {
                    self.phase = Phase::Failed;
                    actions.push(Action::Publish(Event::new(
                        PROTOCOL_FAILED,
                        json!({
                            "protocol": "acp",
                            "session_id": self.session_id,
                            "error": message.get("error").cloned().unwrap_or(Value::Null),
                        }),
                    )));
                } else {
                    self.phase = Phase::Initialized;
                    let cwd = Value::String(self.workdir.to_string_lossy().into_owned());
                    actions.push(self.request(
                        Pending::NewSession,
                        "session/new",
                        &json!({ "cwd": cwd, "mcpServers": [] }),
                    ));
                }
            },
            Some(Pending::NewSession) => {
                if message.get("error").is_some() {
                    self.phase = Phase::Failed;
                    actions.push(Action::Publish(Event::new(
                        PROTOCOL_FAILED,
                        json!({
                            "protocol": "acp",
                            "session_id": self.session_id,
                            "error": message.get("error").cloned().unwrap_or(Value::Null),
                        }),
                    )));
                } else {
                    self.acp_session_id = message
                        .get("result")
                        .and_then(|result| result.get("sessionId"))
                        .and_then(Value::as_str)
                        .map(String::from);
                    if self.acp_session_id.is_some() {
                        self.phase = Phase::Ready;
                        actions.push(Action::Publish(Event::new(
                            PROTOCOL_READY,
                            json!({ "protocol": "acp", "session_id": self.session_id }),
                        )));
                    }
                }
            },
            Some(Pending::Prompt) => {
                self.phase = Phase::Ready;
                // A prompt response that carries an error is a failed turn, not
                // a completed one: reporting it as completed with a null stop
                // reason would claim the model answered.
                actions.push(Action::Publish(match message.get("error") {
                    Some(error) => Event::new(
                        PROMPT_FAILED,
                        json!({
                            "protocol": "acp",
                            "session_id": self.session_id,
                            "error": error,
                        }),
                    ),
                    None => Event::new(
                        PROMPT_COMPLETED,
                        json!({
                            "protocol": "acp",
                            "session_id": self.session_id,
                            "stop_reason": message
                                .get("result")
                                .and_then(|result| result.get("stopReason"))
                                .cloned()
                                .unwrap_or(Value::Null),
                        }),
                    ),
                }));
            },
            None => {},
        }
        actions
    }

    /// Answers a pending permission request under its original id: with the
    /// option the decision selected, or with a cancellation when nobody
    /// decided.
    ///
    /// The outcome follows the event `type`, not the payload: ACP can only
    /// select one of the options the child offered or cancel, so a `granted`
    /// and a `denied` are the same reply to the child and differ only in which
    /// option the approver chose.
    fn on_permission_decision(
        &mut self,
        event: &Event,
    ) -> Vec<Action> {
        let Some(request_id) = event.data.get("request_id").and_then(Value::as_str) else {
            return Vec::new();
        };
        let Some(id) = self.inbound.remove(request_id) else {
            return Vec::new();
        };
        let cancelled = event.r#type == SESSION_PERMISSION_CANCELLED;
        let outcome = match event.data.get("option_id").and_then(Value::as_str) {
            Some(option_id) if !cancelled => {
                json!({ "outcome": "selected", "optionId": option_id })
            },
            _ => json!({ "outcome": "cancelled" }),
        };
        vec![Action::Write(
            json!({
                "jsonrpc": JSONRPC_VERSION,
                "id": id,
                "result": { "outcome": outcome },
            })
            .to_string(),
        )]
    }

    /// Starts a prompt turn from a routed [`PROMPT_REQUESTED`] event.
    fn on_prompt(
        &mut self,
        event: &Event,
    ) -> Vec<Action> {
        let Some(session) = self.acp_session_id.clone() else {
            return Vec::new();
        };
        if self.phase != Phase::Ready {
            return Vec::new();
        }
        let prompt = event.data.get("prompt").cloned().unwrap_or(Value::Null);
        self.phase = Phase::Prompting;
        vec![self.request(
            Pending::Prompt,
            "session/prompt",
            &json!({ "sessionId": session, "prompt": prompt }),
        )]
    }
}

impl Bridge for AcpBridge {
    fn start(&mut self) -> Vec<Action> {
        vec![self.request(
            Pending::Initialize,
            "initialize",
            &json!({
                "protocolVersion": self.protocol_version,
                "clientCapabilities": Self::client_capabilities(),
                "clientInfo": {
                    "name": self.client_name,
                    "version": env!("CARGO_PKG_VERSION"),
                },
            }),
        )]
    }

    fn on_event(
        &mut self,
        event: &Event,
    ) -> Vec<Action> {
        match event.r#type.as_str() {
            PROMPT_REQUESTED => self.on_prompt(event),
            r#type if is_permission_decision(r#type) => self.on_permission_decision(event),
            PROTOCOL_OUTBOUND => {
                if event.data.get("protocol").and_then(Value::as_str) != Some("acp") {
                    return Vec::new();
                }
                let Some(message) = event.data.get("message") else {
                    return Vec::new();
                };
                if message.get("jsonrpc").and_then(Value::as_str) != Some(JSONRPC_VERSION) {
                    return Vec::new();
                }
                vec![Action::Write(message.to_string())]
            },
            _ => Vec::new(),
        }
    }

    fn on_line(
        &mut self,
        line: &str,
    ) -> Vec<Action> {
        let Some(message) = parse_message(line) else {
            return Vec::new();
        };
        if let Some(method) = message.get("method").and_then(Value::as_str) {
            return match message.get("id") {
                Some(id) if !id.is_null() => {
                    let params = message.get("params").cloned().unwrap_or(Value::Null);
                    self.on_request(id, method, &params)
                },
                // A notification (no id) is only published.
                _ => vec![Action::Publish(protocol_inbound("acp", &message))],
            };
        }
        message.get("id").and_then(Value::as_u64).map_or_else(
            || vec![Action::Publish(protocol_inbound("acp", &message))],
            |id| self.on_response(id, &message),
        )
    }
}

/// The string form of a JSON-RPC id, used as the correlation key in events.
fn id_to_string(id: &Value) -> String {
    match id {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AcpProtocol, Bridge, McpProtocol, PROMPT_COMPLETED, PROMPT_FAILED, PROTOCOL_INBOUND,
        PROTOCOL_OUTBOUND, PROTOCOL_READY, Protocol, SESSION_PERMISSION_REQUESTED,
        permission_granted, prompt_requested, session_subject,
    };
    use agentd_events::Event;
    use serde_json::{Value, json};

    fn lines(actions: &[super::Action]) -> Vec<String> {
        actions
            .iter()
            .filter_map(|action| match action {
                super::Action::Write(line) => Some(line.clone()),
                super::Action::Publish(_) => None,
            })
            .collect()
    }

    fn events(actions: &[super::Action]) -> Vec<Event> {
        actions
            .iter()
            .filter_map(|action| match action {
                super::Action::Publish(event) => Some(event.clone()),
                super::Action::Write(_) => None,
            })
            .collect()
    }

    fn parse(line: &str) -> Value {
        serde_json::from_str(line).expect("a JSON line")
    }

    fn connect<P: Protocol>(protocol: &P) -> Box<dyn Bridge> {
        protocol.connect(&super::SessionContext {
            session_id: "s1",
            workdir: std::path::Path::new("/repo"),
        })
    }

    /// An ACP bridge driven through `initialize` and `session/new`, so it
    /// accepts prompts.
    fn ready_bridge() -> Box<dyn Bridge> {
        let mut bridge = connect(&AcpProtocol::default());
        let initialize = parse(&lines(&bridge.start())[0]);
        let initialize_id = initialize["id"].as_u64().expect("an id");
        let actions = bridge.on_line(
            &json!({
                "jsonrpc": "2.0",
                "id": initialize_id,
                "result": { "protocolVersion": 1, "agentCapabilities": {} },
            })
            .to_string(),
        );
        let new_session_id = parse(&lines(&actions)[0])["id"].as_u64().expect("an id");
        let ready = bridge.on_line(
            &json!({
                "jsonrpc": "2.0",
                "id": new_session_id,
                "result": { "sessionId": "sess_1" },
            })
            .to_string(),
        );
        assert!(
            events(&ready)
                .iter()
                .any(|event| event.r#type == PROTOCOL_READY),
            "the bridge should be ready"
        );
        bridge
    }

    #[test]
    fn mcp_start_sends_the_initialize_request() {
        let mut bridge = connect(&McpProtocol::default());
        let request = parse(&lines(&bridge.start())[0]);

        assert_eq!(request["jsonrpc"], "2.0");
        assert_eq!(request["method"], "initialize");
        assert_eq!(request["params"]["protocolVersion"], "2024-11-05");
        assert_eq!(request["params"]["clientInfo"]["name"], "agentd");
    }

    #[test]
    fn mcp_uplink_parses_a_notification_into_an_event() {
        let mut bridge = connect(&McpProtocol::default());
        let actions =
            bridge.on_line(r#"{"jsonrpc":"2.0","method":"notifications/tools/list_changed"}"#);

        let published = events(&actions);
        assert_eq!(published[0].r#type, PROTOCOL_INBOUND);
        assert_eq!(published[0].data["protocol"], "mcp");
        assert_eq!(
            published[0].data["message"]["method"],
            "notifications/tools/list_changed"
        );
        assert!(lines(&actions).is_empty());
    }

    #[test]
    fn mcp_uplink_acknowledges_the_initialize_response() {
        let mut bridge = connect(&McpProtocol::default());
        let actions =
            bridge.on_line(r#"{"jsonrpc":"2.0","id":0,"result":{"protocolVersion":"2024-11-05"}}"#);

        let reply = parse(&lines(&actions)[0]);
        assert_eq!(reply["method"], "notifications/initialized");
    }

    #[test]
    fn mcp_ignores_blank_non_json_and_foreign_messages() {
        let mut bridge = connect(&McpProtocol::default());

        assert!(bridge.on_line("   ").is_empty());
        assert!(bridge.on_line("not json").is_empty());
        assert!(bridge.on_line(r#"{"method":"no-version"}"#).is_empty());
    }

    #[test]
    fn mcp_downlink_renders_only_this_protocols_messages() {
        let mut bridge = connect(&McpProtocol::default());
        let ours = Event::new(
            PROTOCOL_OUTBOUND,
            json!({
                "protocol": "mcp",
                "message": { "jsonrpc": "2.0", "id": 1, "method": "ping" },
            }),
        );
        let foreign = Event::new(
            PROTOCOL_OUTBOUND,
            json!({
                "protocol": "acp",
                "message": { "jsonrpc": "2.0", "id": 1, "method": "ping" },
            }),
        );

        assert_eq!(parse(&lines(&bridge.on_event(&ours))[0])["method"], "ping");
        assert!(bridge.on_event(&foreign).is_empty());
        assert!(
            bridge
                .on_event(&Event::new("agent.inbox", json!({})))
                .is_empty()
        );
    }

    #[test]
    fn acp_negotiates_a_session_and_a_prompt() {
        let mut bridge = connect(&AcpProtocol::default());

        let initialize = parse(&lines(&bridge.start())[0]);
        assert_eq!(initialize["method"], "initialize");
        assert_eq!(initialize["params"]["protocolVersion"], 1);
        assert_eq!(
            initialize["params"]["clientCapabilities"]["fs"]["readTextFile"],
            false
        );
        assert_eq!(
            initialize["params"]["clientCapabilities"]["terminal"],
            false
        );
        let initialize_id = initialize["id"].as_u64().expect("an id");

        let actions = bridge.on_line(
            &json!({
                "jsonrpc": "2.0",
                "id": initialize_id,
                "result": { "protocolVersion": 1, "agentCapabilities": {} },
            })
            .to_string(),
        );
        let new_session = parse(&lines(&actions)[0]);
        assert_eq!(new_session["method"], "session/new");
        assert_eq!(new_session["params"]["mcpServers"], json!([]));
        let new_session_id = new_session["id"].as_u64().expect("an id");

        let actions = bridge.on_line(
            &json!({
                "jsonrpc": "2.0",
                "id": new_session_id,
                "result": { "sessionId": "sess_1" },
            })
            .to_string(),
        );
        assert!(
            events(&actions)
                .iter()
                .any(|event| event.r#type == PROTOCOL_READY)
        );

        let prompt = prompt_requested(&json!([{ "type": "text", "text": "hi" }]));
        let turn = parse(&lines(&bridge.on_event(&prompt))[0]);
        assert_eq!(turn["method"], "session/prompt");
        assert_eq!(turn["params"]["sessionId"], "sess_1");
        assert_eq!(turn["params"]["prompt"][0]["text"], "hi");
    }

    #[test]
    fn acp_reports_a_failed_prompt_as_a_failure_not_a_completion() {
        let mut bridge = ready_bridge();
        let prompt = prompt_requested(&json!([{ "type": "text", "text": "hi" }]));
        let turn = parse(&lines(&bridge.on_event(&prompt))[0]);
        let prompt_id = turn["id"].as_u64().expect("an id");

        let actions = bridge.on_line(
            &json!({
                "jsonrpc": "2.0",
                "id": prompt_id,
                "error": { "code": -32000, "message": "the provider refused" },
            })
            .to_string(),
        );

        let published = events(&actions);
        let failed: Vec<&Event> = published
            .iter()
            .filter(|event| event.r#type == PROMPT_FAILED)
            .collect();
        assert_eq!(failed.len(), 1, "{published:?}");
        assert_eq!(failed[0].data["error"]["code"], -32000);
        assert_eq!(failed[0].data.get("stop_reason"), None);
        assert!(
            published
                .iter()
                .all(|event| event.r#type != PROMPT_COMPLETED),
            "a failed turn is not a completed one: {published:?}"
        );
    }

    #[test]
    fn acp_reports_a_finished_prompt_with_its_stop_reason() {
        let mut bridge = ready_bridge();
        let prompt = prompt_requested(&json!([{ "type": "text", "text": "hi" }]));
        let turn = parse(&lines(&bridge.on_event(&prompt))[0]);
        let prompt_id = turn["id"].as_u64().expect("an id");

        let actions = bridge.on_line(
            &json!({
                "jsonrpc": "2.0",
                "id": prompt_id,
                "result": { "stopReason": "end_turn" },
            })
            .to_string(),
        );

        let published = events(&actions);
        let completed: Vec<&Event> = published
            .iter()
            .filter(|event| event.r#type == PROMPT_COMPLETED)
            .collect();
        assert_eq!(completed.len(), 1, "{published:?}");
        assert_eq!(completed[0].data["stop_reason"], "end_turn");
        assert!(
            published.iter().all(|event| event.r#type != PROMPT_FAILED),
            "a turn with a stop reason did not fail: {published:?}"
        );
    }

    #[test]
    fn acp_does_not_prompt_before_a_session_exists() {
        let mut bridge = connect(&AcpProtocol::default());
        let request = prompt_requested(&json!([]));

        assert!(bridge.on_event(&request).is_empty());
    }

    #[test]
    fn acp_routes_a_permission_request_and_answers_under_the_same_id() {
        let mut bridge = connect(&AcpProtocol::default());

        let actions = bridge.on_line(
            &json!({
                "jsonrpc": "2.0",
                "id": 5,
                "method": "session/request_permission",
                "params": {
                    "sessionId": "sess_1",
                    "toolCall": { "toolCallId": "call_1" },
                    "options": [{ "optionId": "allow-once", "name": "Allow", "kind": "allow_once" }],
                },
            })
            .to_string(),
        );
        let requested = events(&actions)
            .into_iter()
            .find(|event| event.r#type == SESSION_PERMISSION_REQUESTED)
            .expect("a permission request");
        assert_eq!(requested.data["request_id"], "5");
        assert_eq!(requested.data["tool_call"]["toolCallId"], "call_1");

        let decision = permission_granted("5", "allow-once").with_subject(session_subject("s1"));
        let response = parse(&lines(&bridge.on_event(&decision))[0]);
        assert_eq!(response["id"], 5);
        assert_eq!(response["result"]["outcome"]["outcome"], "selected");
        assert_eq!(response["result"]["outcome"]["optionId"], "allow-once");

        // A second decision for the same id is not re-answered.
        assert!(bridge.on_event(&decision).is_empty());
    }

    #[test]
    fn acp_ignores_unknown_ids_and_blank_lines() {
        let mut bridge = connect(&AcpProtocol::default());

        assert!(bridge.on_line("").is_empty());
        assert_eq!(
            bridge
                .on_line(r#"{"jsonrpc":"2.0","id":99,"result":{}}"#)
                .len(),
            1
        );
    }
}
