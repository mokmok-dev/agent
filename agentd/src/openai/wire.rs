//! The wire format an OpenAI-compatible `chat/completions` endpoint speaks.
//!
//! The mapping lives here and only here: the agent's own shapes ([`Message`],
//! [`Tool`], [`Response`]) never learn the words `role`, `tool_calls`, or the
//! provider's error envelope. See `docs/session/agent.md`.
//!
//! Two details of this protocol are easy to get wrong.
//!
//! - A tool call's `function.arguments` is a **JSON string**, not an object. The
//!   mapping serializes the agent's [`Value`] on the way out and parses the string
//!   back on the way in. A string that is not JSON is handed on as a string rather
//!   than dropped, because the model did say something: the capability reports it
//!   back as a malformed command, and the model gets to correct itself.
//! - The reply is a *provider's*, so it carries fields this project does not know
//!   (`id`, `usage`, `finish_reason`, ...). The reply types therefore accept
//!   unknown fields. Only the request is exact, because the agent is its only
//!   writer.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::{Message, Response, Tool, ToolCall};

use super::Error;

/// The body of one `chat/completions` request.
#[derive(Debug, Serialize)]
pub(super) struct Request<'a> {
    /// The model id the endpoint is asked for.
    model: &'a str,
    /// The conversation so far.
    messages: Vec<WireMessage<'a>>,
    /// The tools the model may call.
    tools: Vec<WireTool<'a>>,
    /// Always `false`: the agent's `Model` seam answers with a whole turn, so a
    /// streamed reply has nothing to land in. See `docs/session/agent.md`.
    stream: bool,
}

impl<'a> Request<'a> {
    /// The body that asks `model` for the next turn of `messages`, offering
    /// `tools`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Request`] if a tool call's arguments cannot be serialized,
    /// which a [`Value`] built from JSON cannot cause.
    pub(super) fn new(
        model: &'a str,
        messages: &'a [Message],
        tools: &'a [Tool],
    ) -> Result<Self, Error> {
        let messages = messages
            .iter()
            .map(WireMessage::from_message)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            model,
            messages,
            tools: tools.iter().map(WireTool::from_tool).collect(),
            stream: false,
        })
    }

    /// The body as the bytes of a JSON document.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Request`] if the body cannot be serialized.
    pub(super) fn to_bytes(&self) -> Result<Vec<u8>, Error> {
        serde_json::to_vec(self).map_err(|error| Error::Request(error.to_string()))
    }
}

/// One message in the provider's shape.
#[derive(Debug, Serialize)]
struct WireMessage<'a> {
    /// `system`, `user`, `assistant`, or `tool`.
    role: &'static str,
    /// What the message says. Absent for a turn that only calls tools.
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<&'a str>,
    /// The tools an assistant turn asked for.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<WireCall<'a>>,
    /// The call a `tool` message answers.
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<&'a str>,
}

impl<'a> WireMessage<'a> {
    /// The provider's form of `message`.
    fn from_message(message: &'a Message) -> Result<Self, Error> {
        Ok(match message {
            Message::System { content } => Self::text("system", content),
            Message::User { content } => Self::text("user", content),
            Message::Assistant { content, calls } => Self {
                role: "assistant",
                content: Some(content),
                tool_calls: calls
                    .iter()
                    .map(WireCall::from_call)
                    .collect::<Result<Vec<_>, _>>()?,
                tool_call_id: None,
            },
            Message::Tool { call_id, content } => Self {
                role: "tool",
                content: Some(content),
                tool_calls: Vec::new(),
                tool_call_id: Some(call_id),
            },
        })
    }

    /// A message that is only text.
    const fn text(
        role: &'static str,
        content: &'a str,
    ) -> Self {
        Self {
            role,
            content: Some(content),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }
}

/// One tool call an assistant turn emitted.
#[derive(Debug, Serialize)]
struct WireCall<'a> {
    /// The provider's identifier, which the tool's result must name.
    id: &'a str,
    /// Always `function`: it is the only kind of tool this agent offers.
    #[serde(rename = "type")]
    kind: &'static str,
    /// What the model asked for.
    function: WireFunction<'a>,
}

impl<'a> WireCall<'a> {
    /// The provider's form of `call`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Request`] if the arguments cannot be serialized.
    fn from_call(call: &'a ToolCall) -> Result<Self, Error> {
        Ok(Self {
            id: &call.id,
            kind: "function",
            function: WireFunction {
                name: &call.name,
                arguments: serde_json::to_string(&call.arguments)
                    .map_err(|error| Error::Request(error.to_string()))?,
            },
        })
    }
}

/// What a tool call names and asks for.
#[derive(Debug, Serialize)]
struct WireFunction<'a> {
    /// The tool's name.
    name: &'a str,
    /// The arguments as a JSON **string**, which is the form this protocol uses.
    arguments: String,
}

/// One tool the model may call.
#[derive(Debug, Serialize)]
struct WireTool<'a> {
    /// Always `function`.
    #[serde(rename = "type")]
    kind: &'static str,
    /// The tool itself.
    function: WireToolFunction<'a>,
}

impl<'a> WireTool<'a> {
    /// The provider's form of `tool`.
    fn from_tool(tool: &'a Tool) -> Self {
        Self {
            kind: "function",
            function: WireToolFunction {
                name: &tool.name,
                description: &tool.description,
                parameters: &tool.parameters,
            },
        }
    }
}

/// The name, description, and schema of one tool.
#[derive(Debug, Serialize)]
struct WireToolFunction<'a> {
    /// The name the model calls it by.
    name: &'a str,
    /// What it does, which is how the model decides whether to call it.
    description: &'a str,
    /// Its arguments, as a JSON schema.
    parameters: &'a Value,
}

/// A reply from the endpoint.
///
/// Unknown fields are accepted: the reply is the provider's document, and a
/// provider may add to it at any time. The fields this agent does not read are
/// dropped, which is the point of mapping into the agent's own shapes.
#[derive(Debug, Deserialize)]
pub(super) struct Reply {
    /// One entry per model turn. This client asks for one, so it reads the first.
    #[serde(default)]
    choices: Vec<Choice>,
    /// The provider's error envelope, which some endpoints send with a `2xx`.
    #[serde(default)]
    error: Option<ApiError>,
}

/// One turn of the model's answer.
#[derive(Debug, Deserialize)]
struct Choice {
    /// What the model said and asked for.
    message: ReplyMessage,
}

/// What the model said in one turn.
#[derive(Debug, Default, Deserialize)]
struct ReplyMessage {
    /// Its prose, which is `null` on a turn that only calls tools.
    #[serde(default)]
    content: Option<String>,
    /// The tools it asked for.
    #[serde(default)]
    tool_calls: Vec<ReplyCall>,
}

/// One tool call a reply asked for.
#[derive(Debug, Deserialize)]
struct ReplyCall {
    /// The identifier the result must name.
    id: String,
    /// What it asked for.
    function: ReplyFunction,
}

/// The tool a reply asked for.
#[derive(Debug, Deserialize)]
struct ReplyFunction {
    /// The tool's name.
    name: String,
    /// The arguments as a JSON string, per this protocol.
    #[serde(default)]
    arguments: Option<String>,
}

/// The provider's error envelope.
#[derive(Debug, Deserialize)]
struct ApiError {
    /// What the provider said went wrong.
    #[serde(default)]
    message: Option<String>,
}

impl Reply {
    /// The agent's [`Response`] for this reply.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Reply`] when the provider sent an error envelope with a
    /// `2xx`, or when the reply carries no choice to read.
    pub(super) fn into_response(self) -> Result<Response, Error> {
        if let Some(error) = &self.error {
            return Err(Error::Reply(error.reason()));
        }
        let Some(choice) = self.choices.into_iter().next() else {
            return Err(Error::Reply("the reply carried no choices".to_owned()));
        };
        let calls = choice
            .message
            .tool_calls
            .into_iter()
            .map(|call| ToolCall {
                id: call.id,
                name: call.function.name,
                arguments: parse_arguments(call.function.arguments),
            })
            .collect();
        Ok(Response {
            content: choice.message.content.unwrap_or_default(),
            calls,
        })
    }

    /// The provider's own message, when the reply is an error envelope that carries
    /// one.
    ///
    /// A caller that has already refused the reply for its status uses this to quote
    /// the provider rather than the transport.
    pub(super) fn error_reason(&self) -> Option<String> {
        self.error.as_ref().and_then(|error| error.message.clone())
    }
}

impl ApiError {
    /// The provider's message, or a phrase when it sent none.
    fn reason(&self) -> String {
        self.message
            .clone()
            .unwrap_or_else(|| "the endpoint reported an error".to_owned())
    }
}

/// Parse `body` as a reply.
///
/// # Errors
///
/// Returns [`Error::Reply`] when the body is not the JSON document this protocol
/// describes.
pub(super) fn parse(body: &[u8]) -> Result<Reply, Error> {
    serde_json::from_slice(body).map_err(|error| Error::Reply(error.to_string()))
}

/// What the model said a tool's arguments are.
///
/// A string that is not JSON is handed on as a string: the model said something,
/// and a capability reports it back as a malformed command rather than the turn
/// failing.
fn parse_arguments(arguments: Option<String>) -> Value {
    arguments.map_or(Value::Null, |text| {
        serde_json::from_str(&text).unwrap_or(Value::String(text))
    })
}

#[cfg(test)]
mod tests {
    // Tests for the mapping in both directions, and for the two details that are
    // easy to get wrong: arguments as a JSON string, and a reply that carries
    // fields this project does not know.

    use serde_json::json;

    use super::*;

    /// The request body for one turn, as JSON.
    fn request(
        messages: &[Message],
        tools: &[Tool],
    ) -> Value {
        let request = Request::new("grok-4.7", messages, tools).expect("builds");
        let bytes = request.to_bytes().expect("serializes");
        serde_json::from_slice(&bytes).expect("is JSON")
    }

    /// The reply for `body`.
    fn reply(body: &Value) -> Result<Response, Error> {
        parse(&serde_json::to_vec(body).expect("serializes"))?.into_response()
    }

    #[test]
    fn the_request_names_the_model_and_asks_for_a_whole_turn() {
        let body = request(&[], &[]);
        assert_eq!(body["model"], "grok-4.7");
        assert_eq!(body["stream"], json!(false));
    }

    #[test]
    fn the_system_and_user_messages_carry_their_role_and_content() {
        let body = request(
            &[
                Message::System {
                    content: "be brief".to_owned(),
                },
                Message::User {
                    content: "list the files".to_owned(),
                },
            ],
            &[],
        );
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][0]["content"], "be brief");
        assert_eq!(body["messages"][1]["role"], "user");
        assert_eq!(body["messages"][1]["content"], "list the files");
    }

    #[test]
    fn an_assistant_turn_carries_its_tool_calls_with_arguments_as_a_json_string() {
        let body = request(
            &[Message::Assistant {
                content: "checking".to_owned(),
                calls: vec![ToolCall {
                    id: "call-1".to_owned(),
                    name: "shell".to_owned(),
                    arguments: json!({"argv": ["ls", "-l"]}),
                }],
            }],
            &[],
        );
        let call = &body["messages"][0]["tool_calls"][0];
        assert_eq!(call["id"], "call-1");
        assert_eq!(call["type"], "function");
        assert_eq!(call["function"]["name"], "shell");
        // The protocol carries the arguments as a string, not an object.
        let arguments = call["function"]["arguments"]
            .as_str()
            .expect("arguments is a string");
        assert_eq!(
            serde_json::from_str::<Value>(arguments).expect("is JSON"),
            json!({"argv": ["ls", "-l"]})
        );
    }

    #[test]
    fn a_tool_result_names_the_call_it_answers() {
        let body = request(
            &[Message::Tool {
                call_id: "call-1".to_owned(),
                content: "the exit code was 0".to_owned(),
            }],
            &[],
        );
        assert_eq!(body["messages"][0]["role"], "tool");
        assert_eq!(body["messages"][0]["tool_call_id"], "call-1");
        assert_eq!(body["messages"][0]["content"], "the exit code was 0");
        assert!(
            body["messages"][0].get("tool_calls").is_none(),
            "a tool result names no calls"
        );
    }

    #[test]
    fn a_turn_that_only_calls_tools_still_names_the_assistant_role() {
        let body = request(
            &[Message::Assistant {
                content: String::new(),
                calls: Vec::new(),
            }],
            &[],
        );
        assert_eq!(body["messages"][0]["role"], "assistant");
        assert_eq!(body["messages"][0]["content"], "");
        assert!(
            body["messages"][0].get("tool_calls").is_none(),
            "an empty call list is left out"
        );
    }

    #[test]
    fn the_tools_are_nested_under_a_function() {
        let body = request(
            &[],
            &[Tool {
                name: "shell".to_owned(),
                description: "run a program".to_owned(),
                parameters: json!({"type": "object", "required": ["argv"]}),
            }],
        );
        let tool = &body["tools"][0];
        assert_eq!(tool["type"], "function");
        assert_eq!(tool["function"]["name"], "shell");
        assert_eq!(tool["function"]["description"], "run a program");
        assert_eq!(tool["function"]["parameters"]["required"][0], "argv");
    }

    #[test]
    fn a_reply_maps_to_the_content_and_calls() {
        let response = reply(&json!({
            "choices": [{
                "message": {
                    "content": "here is the listing",
                    "tool_calls": [{
                        "id": "call-9",
                        "type": "function",
                        "function": {"name": "shell", "arguments": "{\"argv\":[\"ls\"]}"}
                    }]
                }
            }]
        }))
        .expect("maps");
        assert_eq!(response.content, "here is the listing");
        assert_eq!(response.calls.len(), 1);
        assert_eq!(response.calls[0].id, "call-9");
        assert_eq!(response.calls[0].name, "shell");
        assert_eq!(response.calls[0].arguments, json!({"argv": ["ls"]}));
    }

    #[test]
    fn a_reply_that_only_calls_tools_has_no_content() {
        let response = reply(&json!({
            "choices": [{"message": {"content": null, "tool_calls": [
                {"id": "c", "function": {"name": "shell", "arguments": "{}"}}
            ]}}]
        }))
        .expect("maps");
        assert_eq!(response.content, "");
        assert_eq!(response.calls.len(), 1);
    }

    #[test]
    fn a_reply_with_no_tool_calls_is_the_answer() {
        let response =
            reply(&json!({"choices": [{"message": {"content": "done"}}]})).expect("maps");
        assert_eq!(response.content, "done");
        assert!(response.calls.is_empty());
    }

    #[test]
    fn arguments_that_are_not_json_are_handed_on_as_a_string() {
        let response = reply(&json!({
            "choices": [{"message": {"tool_calls": [
                {"id": "c", "function": {"name": "shell", "arguments": "not json"}}
            ]}}]
        }))
        .expect("maps");
        assert_eq!(response.calls[0].arguments, json!("not json"));
    }

    #[test]
    fn a_missing_arguments_field_is_null() {
        let response = reply(&json!({
            "choices": [{"message": {"tool_calls": [
                {"id": "c", "function": {"name": "shell"}}
            ]}}]
        }))
        .expect("maps");
        assert_eq!(response.calls[0].arguments, Value::Null);
    }

    #[test]
    fn a_reply_carrying_fields_this_project_does_not_know_is_read() {
        // The reply is the provider's document; it adds fields whenever it likes.
        let response = reply(&json!({
            "id": "chatcmpl-1",
            "object": "chat.completion",
            "created": 1,
            "model": "grok-4.7",
            "system_fingerprint": "fp_1",
            "usage": {"prompt_tokens": 1, "completion_tokens": 2},
            "choices": [{
                "index": 0,
                "finish_reason": "stop",
                "logprobs": null,
                "message": {"role": "assistant", "content": "hello", "refusal": null}
            }]
        }))
        .expect("maps");
        assert_eq!(response.content, "hello");
    }

    #[test]
    fn an_empty_choice_list_is_refused() {
        let error = reply(&json!({"choices": []})).expect_err("refused");
        assert!(
            matches!(&error, Error::Reply(reason) if reason.contains("no choices")),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn an_error_envelope_with_a_success_status_is_refused() {
        let error = reply(&json!({
            "error": {"message": "the model is overloaded", "type": "server_error"},
            "choices": []
        }))
        .expect_err("refused");
        assert!(
            matches!(&error, Error::Reply(reason) if reason == "the model is overloaded"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn an_error_envelope_without_a_message_still_reads() {
        let error = reply(&json!({"error": {}, "choices": []})).expect_err("refused");
        assert!(
            matches!(&error, Error::Reply(reason) if reason == "the endpoint reported an error"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn a_body_that_is_not_json_is_refused() {
        let error = parse(b"<html>gone</html>").expect_err("refused");
        assert!(
            matches!(error, Error::Reply(_)),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn a_replies_error_envelope_is_readable_without_consuming_it() {
        // A caller that refuses the reply for its status quotes the provider from
        // here instead of the transport.
        let reply = parse(br#"{"error": {"message": "invalid api key"}}"#).expect("parses");
        assert_eq!(reply.error_reason().as_deref(), Some("invalid api key"));
    }

    #[test]
    fn a_reply_without_an_error_envelope_has_no_reason() {
        let reply = parse(br#"{"choices": []}"#).expect("parses");
        assert_eq!(reply.error_reason(), None);
    }
}
