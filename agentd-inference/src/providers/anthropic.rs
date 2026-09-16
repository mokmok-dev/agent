//! The Anthropic messages adapter.
//!
//! Anthropic's API differs from the `OpenAI` shape: the system prompt is
//! top-level, tool calls are `tool_use` content blocks, tool results are
//! `tool_result` blocks inside a user turn, and `max_tokens` is required. The
//! stream is reassembled into the same [`Delta`] contract.

use std::collections::BTreeMap;

use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{Value, json};

use super::sse::SseParser;
use crate::provider::{InferenceStream, Provider, ProviderError};
use crate::wire::{Delta, InferenceRequest, Message, Role, ToolSpec};

/// The base URL used when the config names none.
const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

/// The Anthropic API version header value.
const API_VERSION: &str = "2023-06-01";

/// The `max_tokens` sent when a request does not carry one; Anthropic requires
/// the field.
const DEFAULT_MAX_TOKENS: u32 = 8192;

/// An Anthropic provider.
#[derive(Debug)]
pub struct AnthropicProvider {
    client: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
}

impl AnthropicProvider {
    /// Creates a provider for `base_url` authenticating with `api_key`.
    #[must_use]
    pub fn new(
        base_url: Option<String>,
        api_key: Option<String>,
    ) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.unwrap_or_else(|| String::from(DEFAULT_BASE_URL)),
            api_key,
        }
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    async fn stream(
        &self,
        request: InferenceRequest,
    ) -> Result<InferenceStream, ProviderError> {
        let Some(api_key) = &self.api_key else {
            return Err(ProviderError::Failed(String::from(
                "the anthropic provider requires an API key",
            )));
        };
        let (system, messages) = anthropic_messages(&request.messages);
        let mut body = json!({
            "model": request.model.clone().unwrap_or_default(),
            "max_tokens": DEFAULT_MAX_TOKENS,
            "messages": messages,
            "stream": true,
        });
        if !system.is_empty() {
            body["system"] = json!(system);
        }
        if !request.tools.is_empty() {
            body["tools"] = json!(anthropic_tools(&request.tools));
        }

        let url = format!("{}/v1/messages", self.base_url.trim_end_matches('/'));
        let response = self
            .client
            .post(url)
            .header("x-api-key", api_key)
            .header("anthropic-version", API_VERSION)
            .json(&body)
            .send()
            .await
            .map_err(|error| {
                ProviderError::Failed(format!("the request to the provider failed: {error}"))
            })?;
        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(ProviderError::Failed(format!(
                "the provider returned {status}: {text}"
            )));
        }

        Ok(parse_stream(response.bytes_stream()))
    }
}

/// Reassembles the provider's SSE stream into deltas.
fn parse_stream<S, B, E>(mut byte_stream: S) -> InferenceStream
where
    S: futures_util::Stream<Item = Result<B, E>> + Send + Unpin + 'static,
    B: AsRef<[u8]> + Send,
    E: std::fmt::Display + Send,
{
    Box::pin(async_stream::stream! {
        let mut parser = SseParser::new();
        let mut calls: BTreeMap<u32, PartialCall> = BTreeMap::new();
        let mut finish_reason = None;
        while let Some(chunk) = byte_stream.next().await {
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(error) => {
                    yield Err(ProviderError::Failed(format!("the stream failed: {error}")));
                    return;
                },
            };
            let events = match parser.push(chunk.as_ref()) {
                Ok(events) => events,
                Err(error) => {
                    yield Err(error);
                    return;
                },
            };
            for event in events {
                let value: Value = match serde_json::from_str(event.data.trim()) {
                    Ok(value) => value,
                    Err(error) => {
                        yield Err(ProviderError::Failed(format!(
                            "the provider sent an invalid event: {error}"
                        )));
                        return;
                    },
                };
                match value.get("type").and_then(Value::as_str) {
                    Some("content_block_start") => {
                        start_block(&mut calls, &value);
                    },
                    Some("content_block_delta") => {
                        if let Some(delta) = content_delta(&mut calls, &value) {
                            yield Ok(delta);
                        }
                    },
                    Some("message_delta") => {
                        if let Some(reason) = value.pointer("/delta/stop_reason").and_then(Value::as_str) {
                            finish_reason = Some(String::from(reason));
                        }
                    },
                    Some("message_stop") => {
                        for call in std::mem::take(&mut calls).into_values() {
                            if let Some(delta) = call.into_delta() {
                                yield Ok(delta);
                            }
                        }
                        yield Ok(Delta::Done { finish_reason: finish_reason.take() });
                        return;
                    },
                    Some("error") => {
                        let message = value
                            .pointer("/error/message")
                            .and_then(Value::as_str)
                            .unwrap_or("the provider reported an error")
                            .to_string();
                        yield Err(ProviderError::Failed(message));
                        return;
                    },
                    _ => {},
                }
            }
        }
        for call in std::mem::take(&mut calls).into_values() {
            if let Some(delta) = call.into_delta() {
                yield Ok(delta);
            }
        }
        yield Ok(Delta::Done { finish_reason });
    })
}

/// Records a `tool_use` block's identity at its start.
fn start_block(
    calls: &mut BTreeMap<u32, PartialCall>,
    value: &Value,
) {
    if value.pointer("/content_block/type").and_then(Value::as_str) != Some("tool_use") {
        return;
    }
    let index = block_index(value);
    let entry = calls.entry(index).or_default();
    if let Some(id) = value.pointer("/content_block/id").and_then(Value::as_str) {
        id.clone_into(&mut entry.id);
    }
    if let Some(name) = value.pointer("/content_block/name").and_then(Value::as_str) {
        name.clone_into(&mut entry.name);
    }
}

/// Handles a content-block delta, returning a text delta when it carries text.
fn content_delta(
    calls: &mut BTreeMap<u32, PartialCall>,
    value: &Value,
) -> Option<Delta> {
    match value.pointer("/delta/type").and_then(Value::as_str) {
        Some("text_delta") => {
            let text = value.pointer("/delta/text").and_then(Value::as_str)?;
            (!text.is_empty()).then(|| Delta::Text {
                text: String::from(text),
            })
        },
        Some("input_json_delta") => {
            let fragment = value
                .pointer("/delta/partial_json")
                .and_then(Value::as_str)
                .unwrap_or_default();
            calls
                .entry(block_index(value))
                .or_default()
                .arguments
                .push_str(fragment);
            None
        },
        _ => None,
    }
}

/// The content-block index of an event, defaulting to zero.
fn block_index(value: &Value) -> u32 {
    value
        .get("index")
        .and_then(Value::as_u64)
        .and_then(|index| u32::try_from(index).ok())
        .unwrap_or(0)
}

/// A tool call being reassembled from streamed fragments.
#[derive(Debug, Default)]
struct PartialCall {
    id: String,
    name: String,
    arguments: String,
}

impl PartialCall {
    /// Converts a complete call into a delta, or `None` if it never named a tool.
    fn into_delta(self) -> Option<Delta> {
        if self.name.is_empty() {
            return None;
        }
        Some(Delta::ToolCall {
            id: self.id,
            name: self.name,
            arguments: self.arguments,
        })
    }
}

/// Splits messages into the top-level system prompt and the Anthropic turns.
///
/// Consecutive tool results are merged into one user turn, as the API requires
/// all of a turn's tool results to follow the assistant message together.
fn anthropic_messages(messages: &[Message]) -> (String, Vec<Value>) {
    let mut system = Vec::new();
    let mut turns: Vec<Value> = Vec::new();
    let mut tool_results: Vec<Value> = Vec::new();
    for message in messages {
        if message.role == Role::Tool {
            tool_results.push(json!({
                "type": "tool_result",
                "tool_use_id": message.tool_call_id.as_deref().unwrap_or_default(),
                "content": message.content,
            }));
            continue;
        }
        flush_tool_results(&mut tool_results, &mut turns);
        match message.role {
            Role::System => system.push(message.content.clone()),
            Role::User => turns.push(json!({ "role": "user", "content": message.content })),
            Role::Assistant => turns.push(assistant_turn(message)),
            Role::Tool => {},
        }
    }
    flush_tool_results(&mut tool_results, &mut turns);
    (system.join("\n\n"), turns)
}

/// Appends any pending tool results as one user turn.
fn flush_tool_results(
    pending: &mut Vec<Value>,
    turns: &mut Vec<Value>,
) {
    if !pending.is_empty() {
        turns.push(json!({ "role": "user", "content": std::mem::take(pending) }));
    }
}

/// Builds an assistant turn, with `tool_use` blocks for any tool calls.
fn assistant_turn(message: &Message) -> Value {
    if message.tool_calls.is_empty() {
        return json!({ "role": "assistant", "content": message.content });
    }
    let mut blocks: Vec<Value> = Vec::new();
    if !message.content.is_empty() {
        blocks.push(json!({ "type": "text", "text": message.content }));
    }
    for call in &message.tool_calls {
        let input: Value = serde_json::from_str(&call.arguments).unwrap_or_else(|_| json!({}));
        blocks.push(json!({
            "type": "tool_use",
            "id": call.id,
            "name": call.name,
            "input": input,
        }));
    }
    json!({ "role": "assistant", "content": blocks })
}

/// Converts tools to the Anthropic `input_schema` format.
fn anthropic_tools(tools: &[ToolSpec]) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| {
            json!({
                "name": tool.name,
                "description": tool.description,
                "input_schema": tool.parameters,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{anthropic_messages, anthropic_tools, parse_stream};
    use crate::wire::{Delta, Message, ToolCall, ToolSpec};
    use futures_util::StreamExt;
    use serde_json::json;

    /// Wraps SSE records into a one-chunk byte stream.
    fn stream_of(records: &str) -> Vec<Result<Vec<u8>, std::io::Error>> {
        vec![Ok(records.as_bytes().to_vec())]
    }

    #[tokio::test]
    async fn reassembles_text_and_tool_use_blocks() {
        let records = concat!(
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"I will \"}}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu-1\",\"name\":\"shell\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"command\\\"\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\":\\\"ls\\\"}\"}}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );

        let deltas: Vec<Delta> = parse_stream(futures_util::stream::iter(stream_of(records)))
            .filter_map(|item| async move { item.ok() })
            .collect()
            .await;

        assert_eq!(
            deltas,
            vec![
                Delta::Text {
                    text: String::from("I will ")
                },
                Delta::ToolCall {
                    id: String::from("toolu-1"),
                    name: String::from("shell"),
                    arguments: String::from(r#"{"command":"ls"}"#),
                },
                Delta::Done {
                    finish_reason: Some(String::from("tool_use")),
                },
            ]
        );
    }

    #[test]
    fn system_becomes_top_level_and_tool_calls_use_blocks() {
        let (system, turns) = anthropic_messages(&[
            Message::system("be helpful"),
            Message::user("list files"),
            Message::with_tool_calls(
                "",
                vec![ToolCall {
                    id: String::from("toolu-1"),
                    name: String::from("shell"),
                    arguments: String::from(r#"{"command":"ls"}"#),
                }],
            ),
        ]);

        assert_eq!(system, "be helpful");
        assert_eq!(turns[0], json!({ "role": "user", "content": "list files" }));
        assert_eq!(
            turns[1],
            json!({
                "role": "assistant",
                "content": [{
                    "type": "tool_use",
                    "id": "toolu-1",
                    "name": "shell",
                    "input": { "command": "ls" },
                }],
            })
        );
    }

    #[test]
    fn consecutive_tool_results_merge_into_one_user_turn() {
        let (_system, turns) = anthropic_messages(&[
            Message::tool("toolu-1", "one"),
            Message::tool("toolu-2", "two"),
        ]);

        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0]["role"], "user");
        assert_eq!(turns[0]["content"].as_array().expect("blocks").len(), 2);
    }

    #[test]
    fn tools_use_the_input_schema_shape() {
        let tools = anthropic_tools(&[ToolSpec {
            name: String::from("shell"),
            description: String::from("run"),
            parameters: json!({ "type": "object" }),
        }]);

        assert_eq!(
            tools,
            vec![json!({
                "name": "shell",
                "description": "run",
                "input_schema": { "type": "object" },
            })]
        );
    }
}
