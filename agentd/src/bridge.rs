//! The protocol-conversion layer between a supervised child process and the
//! event log.
//!
//! A third-party tool speaks its own protocol over stdio; it does not speak
//! `CloudEvents`. A [`Bridge`] is the facade that converts between the two:
//! [`Bridge::uplink`] turns one line of the child's output into an event to
//! append, and [`Bridge::downlink`] turns an event routed to the child into a
//! line to write back. The session manager owns the child and drives the pipes;
//! it never inspects the protocol payload, so adding a protocol does not touch
//! it.
//!
//! [`McpBridge`] is the first protocol: the Model Context Protocol over stdio,
//! which frames each JSON-RPC 2.0 message as one line of JSON.

use agentd_events::Event;
use serde_json::{Value, json};

/// The event type carrying one inbound protocol message, from a bridged child
/// to the log.
pub const BRIDGED_INBOUND: &str = "session.bridge.inbound";
/// The event type carrying one outbound protocol message.
///
/// It is the log-to-child direction, and is distinct from [`BRIDGED_INBOUND`]
/// so an inbound message is never routed straight back to the child that
/// produced it.
pub const BRIDGED_OUTBOUND: &str = "session.bridge.outbound";

/// The prefix of a bridged event's `subject`, addressing one session's child.
pub const SUBJECT_PREFIX: &str = "session:";

/// The `subject` attribute that addresses `session_id`'s child.
#[must_use]
pub fn session_subject(session_id: &str) -> String {
    format!("{SUBJECT_PREFIX}{session_id}")
}

/// The JSON-RPC version every MCP message declares.
const JSONRPC_VERSION: &str = "2.0";

/// One line of child output converted into daemon state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conversion {
    /// The event to append for this line.
    pub event: Event,
    /// Lines the bridge sends to the child in reply, if any (for example a
    /// notification that follows a handshake response).
    pub to_child: Vec<String>,
}

/// Converts between a child's stdio protocol and events.
///
/// Implementations are stateless with respect to the session: the session id
/// and the `subject` attribute are attached by the caller, so a bridge cannot
/// attribute a message to the wrong session.
pub trait Bridge: Send + Sync {
    /// A stable label for the protocol, recorded on every event the bridge
    /// produces.
    fn protocol(&self) -> &'static str;

    /// The lines to write to the child once it has started. Empty by default,
    /// for a protocol with no opening handshake.
    fn handshake(&self) -> Vec<String> {
        Vec::new()
    }

    /// Converts one line of the child's output into a [`Conversion`], or
    /// `None` when the line is incomplete, blank, or not a protocol message.
    fn uplink(
        &self,
        line: &str,
    ) -> Option<Conversion>;

    /// Converts an event routed to the child into a line to write, or `None`
    /// when the event is not a protocol message the bridge can send.
    fn downlink(
        &self,
        event: &Event,
    ) -> Option<String>;
}

/// The Model Context Protocol over stdio: newline-delimited JSON-RPC 2.0.
#[derive(Debug, Clone)]
pub struct McpBridge {
    /// The protocol version sent in the handshake.
    protocol_version: String,
    /// The client name sent in the handshake.
    client_name: String,
}

impl McpBridge {
    /// Creates a bridge that announces `client_name` at
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

    /// The MCP revision this bridge announces by default.
    pub const DEFAULT_PROTOCOL_VERSION: &'static str = "2024-11-05";

    /// Builds the `initialize` request, the first message of an MCP session.
    fn initialize_request(&self) -> String {
        let request = json!({
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
        });
        request.to_string()
    }

    /// Whether `message` is the response to the bridge's `initialize` request,
    /// which an MCP server expects to be followed by `notifications/initialized`.
    fn is_initialize_response(message: &Value) -> bool {
        message.get("id").and_then(Value::as_u64) == Some(0)
            && (message.get("result").is_some() || message.get("error").is_some())
    }

    /// The `notifications/initialized` line that acknowledges a handshake.
    fn initialized_notification() -> String {
        json!({ "jsonrpc": JSONRPC_VERSION, "method": "notifications/initialized" }).to_string()
    }
}

impl Default for McpBridge {
    fn default() -> Self {
        Self::new("agentd")
    }
}

impl Bridge for McpBridge {
    fn protocol(&self) -> &'static str {
        "mcp"
    }

    fn handshake(&self) -> Vec<String> {
        vec![self.initialize_request()]
    }

    fn uplink(
        &self,
        line: &str,
    ) -> Option<Conversion> {
        let message: Value = serde_json::from_str(line).ok()?;
        if message.get("jsonrpc").and_then(Value::as_str) != Some(JSONRPC_VERSION) {
            return None;
        }
        let to_child = if Self::is_initialize_response(&message) {
            vec![Self::initialized_notification()]
        } else {
            Vec::new()
        };
        Some(Conversion {
            event: bridged_inbound(self.protocol(), &message),
            to_child,
        })
    }

    fn downlink(
        &self,
        event: &Event,
    ) -> Option<String> {
        if event.r#type != BRIDGED_OUTBOUND
            || event.data.get("protocol")?.as_str()? != self.protocol()
        {
            return None;
        }
        let message = event.data.get("message")?;
        if message.get("jsonrpc").and_then(Value::as_str) != Some(JSONRPC_VERSION) {
            return None;
        }
        Some(message.to_string())
    }
}

/// Builds the event for one inbound protocol message.
///
/// The `subject` attribute is deliberately not set here: the caller that knows
/// the session attaches it, so a bridge cannot misattribute a message.
#[must_use]
pub fn bridged_inbound(
    protocol: &str,
    message: &Value,
) -> Event {
    Event::new(
        BRIDGED_INBOUND,
        json!({ "protocol": protocol, "message": message }),
    )
}

#[cfg(test)]
mod tests {
    use super::{BRIDGED_INBOUND, BRIDGED_OUTBOUND, Bridge, McpBridge};
    use agentd_events::Event;
    use serde_json::json;

    #[test]
    fn handshake_sends_the_initialize_request() {
        let lines = McpBridge::default().handshake();

        assert_eq!(lines.len(), 1);
        let request: serde_json::Value =
            serde_json::from_str(&lines[0]).expect("the handshake is JSON");
        assert_eq!(request["jsonrpc"], "2.0");
        assert_eq!(request["method"], "initialize");
        assert_eq!(request["params"]["protocolVersion"], "2024-11-05");
        assert_eq!(request["params"]["clientInfo"]["name"], "agentd");
    }

    #[test]
    fn uplink_parses_a_notification_into_an_event() {
        let conversion = McpBridge::default()
            .uplink(r#"{"jsonrpc":"2.0","method":"notifications/tools/list_changed"}"#)
            .expect("a valid message converts");

        assert_eq!(conversion.event.r#type, BRIDGED_INBOUND);
        assert_eq!(conversion.event.data["protocol"], "mcp");
        assert_eq!(
            conversion.event.data["message"]["method"],
            "notifications/tools/list_changed"
        );
        assert!(conversion.to_child.is_empty());
    }

    #[test]
    fn uplink_acknowledges_the_initialize_response() {
        let conversion = McpBridge::default()
            .uplink(r#"{"jsonrpc":"2.0","id":0,"result":{"protocolVersion":"2024-11-05"}}"#)
            .expect("a valid message converts");

        assert_eq!(conversion.to_child.len(), 1);
        let notification: serde_json::Value =
            serde_json::from_str(&conversion.to_child[0]).expect("the reply is JSON");
        assert_eq!(notification["method"], "notifications/initialized");
    }

    #[test]
    fn uplink_ignores_blank_non_json_and_foreign_messages() {
        let bridge = McpBridge::default();

        assert!(bridge.uplink("   ").is_none());
        assert!(bridge.uplink("not json").is_none());
        assert!(bridge.uplink(r#"{"method":"no-version"}"#).is_none());
    }

    #[test]
    fn downlink_renders_only_this_protocols_messages() {
        let bridge = McpBridge::default();
        let ours = Event::new(
            BRIDGED_OUTBOUND,
            json!({
                "protocol": "mcp",
                "message": { "jsonrpc": "2.0", "id": 1, "method": "ping" },
            }),
        );
        let foreign = Event::new(
            BRIDGED_OUTBOUND,
            json!({
                "protocol": "lsp",
                "message": { "jsonrpc": "2.0", "id": 1, "method": "ping" },
            }),
        );

        let line = bridge.downlink(&ours).expect("ours renders");
        let message: serde_json::Value = serde_json::from_str(&line).expect("the line is JSON");
        assert_eq!(message["method"], "ping");
        assert!(bridge.downlink(&foreign).is_none());
        assert!(
            bridge
                .downlink(&Event::new("agent.inbox", json!({})))
                .is_none()
        );
    }
}
