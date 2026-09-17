//! The OpenAI-compatible chat-completions adapter.
//!
//! It speaks the `/chat/completions` streaming API, which `OpenAI`,
//! `OpenRouter`, `Ollama`, `vLLM`, and other compatible servers expose. Text
//! arrives as streamed deltas; tool calls arrive as fragments and are
//! reassembled into complete [`Delta::ToolCall`]s before the terminal delta.

use std::collections::BTreeMap;

use async_trait::async_trait;
use futures_util::StreamExt;
use secrecy::{ExposeSecret as _, SecretString};
use serde_json::{Value, json};

use super::sse::SseParser;
use crate::provider::{InferenceStream, Provider, ProviderError};
use crate::wire::{Delta, InferenceRequest, Message, Role, ToolSpec};

/// The base URL used when the config names none.
const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

/// An OpenAI-compatible provider.
#[derive(Debug)]
pub struct OpenAiProvider {
    client: reqwest::Client,
    base_url: String,
    api_key: Option<SecretString>,
}

impl OpenAiProvider {
    /// Creates a provider for `base_url` authenticating with `api_key`.
    #[must_use]
    pub fn new(
        base_url: Option<&str>,
        api_key: Option<SecretString>,
    ) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.unwrap_or(DEFAULT_BASE_URL).to_owned(),
            api_key,
        }
    }
}

#[async_trait]
impl Provider for OpenAiProvider {
    async fn stream(
        &self,
        request: InferenceRequest,
    ) -> Result<InferenceStream, ProviderError> {
        let mut body = json!({
            "model": request.model.as_deref().unwrap_or_default(),
            "messages": openai_messages(&request.messages),
            "stream": true,
        });
        if !request.tools.is_empty() {
            body["tools"] = json!(openai_tools(&request.tools));
        }

        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let mut builder = self.client.post(url).json(&body);
        if let Some(key) = &self.api_key {
            builder = builder.bearer_auth(key.expose_secret());
        }
        let response = builder.send().await.map_err(|error| {
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
                let data = event.data.trim();
                if data.is_empty() {
                    continue;
                }
                if data == "[DONE]" {
                    for call in std::mem::take(&mut calls).into_values() {
                        if let Some(delta) = call.into_delta() {
                            yield Ok(delta);
                        }
                    }
                    yield Ok(Delta::Done { finish_reason: finish_reason.take() });
                    return;
                }
                let value: Value = match serde_json::from_str(data) {
                    Ok(value) => value,
                    Err(error) => {
                        yield Err(ProviderError::Failed(format!(
                            "the provider sent an invalid event: {error}"
                        )));
                        return;
                    },
                };
                if let Some(message) = value
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .or_else(|| value.get("error").and_then(Value::as_str))
                {
                    yield Err(ProviderError::Failed(String::from(message)));
                    return;
                }
                if let Some(text) = value.pointer("/choices/0/delta/content").and_then(Value::as_str)
                    && !text.is_empty()
                {
                    yield Ok(Delta::Text { text: String::from(text) });
                }
                if let Some(reason) = value.pointer("/choices/0/finish_reason").and_then(Value::as_str) {
                    finish_reason = Some(String::from(reason));
                }
                if let Some(items) = value
                    .pointer("/choices/0/delta/tool_calls")
                    .and_then(Value::as_array)
                {
                    for item in items {
                        accumulate(&mut calls, item);
                    }
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

/// Applies one streamed tool-call fragment.
fn accumulate(
    calls: &mut BTreeMap<u32, PartialCall>,
    item: &Value,
) {
    let index = item
        .get("index")
        .and_then(Value::as_u64)
        .and_then(|index| u32::try_from(index).ok())
        .unwrap_or(0);
    let entry = calls.entry(index).or_default();
    if let Some(id) = item.get("id").and_then(Value::as_str) {
        id.clone_into(&mut entry.id);
    }
    if let Some(name) = item.pointer("/function/name").and_then(Value::as_str) {
        name.clone_into(&mut entry.name);
    }
    if let Some(arguments) = item.pointer("/function/arguments").and_then(Value::as_str) {
        entry.arguments.push_str(arguments);
    }
}

/// Converts messages to the `OpenAI` chat format.
fn openai_messages(messages: &[Message]) -> Vec<Value> {
    messages
        .iter()
        .map(|message| match message.role {
            Role::System => {
                json!({ "role": "system", "content": message.content })
            },
            Role::User => {
                json!({ "role": "user", "content": message.content })
            },
            Role::Assistant => {
                if message.tool_calls.is_empty() {
                    json!({ "role": "assistant", "content": message.content })
                } else {
                    let calls: Vec<Value> = message
                        .tool_calls
                        .iter()
                        .map(|call| {
                            json!({
                                "id": call.id,
                                "type": "function",
                                "function": { "name": call.name, "arguments": call.arguments },
                            })
                        })
                        .collect();
                    json!({
                        "role": "assistant",
                        "content": message.content,
                        "tool_calls": calls,
                    })
                }
            },
            Role::Tool => json!({
                "role": "tool",
                "tool_call_id": message.tool_call_id.as_deref().unwrap_or_default(),
                "content": message.content,
            }),
        })
        .collect()
}

/// Converts tools to the `OpenAI` function format.
fn openai_tools(tools: &[ToolSpec]) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "function": {
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.parameters,
                },
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{OpenAiProvider, openai_messages, openai_tools, parse_stream};
    use crate::wire::{Delta, Message, ToolCall, ToolSpec};
    use futures_util::StreamExt;
    use serde_json::json;

    #[test]
    fn debug_redacts_the_api_key() {
        let provider = OpenAiProvider::new(Some("https://example.test"), Some("sk-secret".into()));

        assert!(!format!("{provider:?}").contains("sk-secret"));
    }

    /// Wraps SSE `data:` records into a one-chunk byte stream.
    fn stream_of(records: &str) -> Vec<Result<Vec<u8>, std::io::Error>> {
        vec![Ok(records.as_bytes().to_vec())]
    }

    #[tokio::test]
    async fn reassembles_text_and_tool_call_fragments() {
        let records = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"I will \"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"run ls\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call-1\",",
            "\"function\":{\"name\":\"shell\",\"arguments\":\"{\\\"command\\\"\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,",
            "\"function\":{\"arguments\":\":\\\"ls\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: [DONE]\n\n",
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
                Delta::Text {
                    text: String::from("run ls")
                },
                Delta::ToolCall {
                    id: String::from("call-1"),
                    name: String::from("shell"),
                    arguments: String::from(r#"{"command":"ls"}"#),
                },
                Delta::Done {
                    finish_reason: Some(String::from("tool_calls")),
                },
            ]
        );
    }

    #[tokio::test]
    async fn a_stream_error_event_is_surfaced() {
        let records = "data: {\"error\":{\"message\":\"context length exceeded\"}}\n\n";

        let items: Vec<_> = parse_stream(futures_util::stream::iter(stream_of(records)))
            .collect()
            .await;

        assert_eq!(items.len(), 1);
        assert!(items[0].is_err());
    }

    #[test]
    fn assistant_tool_calls_use_the_function_shape() {
        let messages = openai_messages(&[Message::with_tool_calls(
            "",
            vec![ToolCall {
                id: String::from("call-1"),
                name: String::from("shell"),
                arguments: String::from(r#"{"command":"ls"}"#),
            }],
        )]);

        assert_eq!(
            messages,
            vec![json!({
                "role": "assistant",
                "content": "",
                "tool_calls": [{
                    "id": "call-1",
                    "type": "function",
                    "function": { "name": "shell", "arguments": "{\"command\":\"ls\"}" },
                }],
            })]
        );
    }

    #[test]
    fn tool_results_carry_the_call_id() {
        let messages = openai_messages(&[Message::tool("call-1", "ok")]);

        assert_eq!(
            messages,
            vec![json!({ "role": "tool", "tool_call_id": "call-1", "content": "ok" })]
        );
    }

    #[test]
    fn tools_are_wrapped_as_functions() {
        let tools = openai_tools(&[ToolSpec {
            name: String::from("shell"),
            description: String::from("run"),
            parameters: json!({ "type": "object" }),
        }]);

        assert_eq!(
            tools,
            vec![json!({
                "type": "function",
                "function": {
                    "name": "shell",
                    "description": "run",
                    "parameters": { "type": "object" },
                },
            })]
        );
    }
}
