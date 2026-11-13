//! OpenAI Chat Completions provider and all OpenAI-compatible endpoints.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use async_stream::try_stream;
use futures_util::StreamExt;
use reqwest::header::HeaderMap;
use serde_json::{Value, json};

use crate::config::{Protocol, ProviderConfig};
use crate::providers::{
    ChatRequest, ChatStream, Provider, SseBuffer, StopReason, StreamEvent, collect_non_stream_body,
    headers, merge_extra_body, post_json, refresh_usage, response_is_sse, truncate_error,
};
use crate::types::{ContentBlock, Message, Role, Usage};

pub struct OpenAiProvider {
    client: reqwest::Client,
    provider_name: String,
    model: String,
    config: ProviderConfig,
    api_key: Option<String>,
    headers: HeaderMap,
}

impl OpenAiProvider {
    pub fn new(
        client: reqwest::Client,
        provider_name: String,
        model: String,
        config: ProviderConfig,
        api_key: Option<String>,
    ) -> Result<Self> {
        let headers = headers(&config, api_key.as_deref())?;
        Ok(Self {
            client,
            provider_name,
            model,
            config,
            api_key,
            headers,
        })
    }

    fn endpoint(&self) -> String {
        let base = self.config.base_url.trim_end_matches('/');
        if base.ends_with("/chat/completions") {
            base.to_string()
        } else {
            format!("{base}/chat/completions")
        }
    }

    fn headers(&self) -> Result<HeaderMap> {
        let mut headers = headers(&self.config, self.api_key.as_deref())?;
        // Custom headers are already included by the shared helper.
        headers.extend(self.headers.clone());
        Ok(headers)
    }
}

#[async_trait::async_trait]
impl Provider for OpenAiProvider {
    fn provider_name(&self) -> &str {
        &self.provider_name
    }

    fn model_name(&self) -> &str {
        &self.model
    }

    fn protocol(&self) -> Protocol {
        Protocol::OpenAi
    }

    async fn stream_chat(&self, request: ChatRequest) -> Result<ChatStream> {
        let mut body = json!({
            "model": self.model,
            "messages": convert_messages(&request.system, &request.messages),
            "stream": true,
        });

        let reasoning_model = is_reasoning_model(&self.model);
        let token_field = self
            .config
            .max_tokens_field
            .as_deref()
            .unwrap_or(if reasoning_model {
                "max_completion_tokens"
            } else {
                "max_tokens"
            });
        body[token_field] = json!(request.max_tokens);

        if !supports_stream_options(&self.provider_name) {
            // Historical compatible servers can reject unknown stream options.
        } else {
            body["stream_options"] = json!({"include_usage": true});
        }

        if !request.tools.is_empty() {
            body["tools"] = Value::Array(
                request
                    .tools
                    .iter()
                    .map(|tool| {
                        json!({
                            "type": "function",
                            "function": {
                                "name": tool.name,
                                "description": tool.description,
                                "parameters": tool.input_schema
                            }
                        })
                    })
                    .collect(),
            );
            body["tool_choice"] = json!("auto");
        }

        if let Some(effort) = request.effort.openai_reasoning()
            && reasoning_model
            && matches!(self.provider_name.as_str(), "openai")
        {
            body["reasoning_effort"] = json!(effort);
        }

        if let Some(temperature) = request.temperature
            && !reasoning_model
        {
            body["temperature"] = json!(temperature);
        }

        merge_extra_body(&self.config, &mut body);
        let response = post_json(&self.client, &self.endpoint(), self.headers()?, &body).await?;

        if !response_is_sse(&response) {
            let body = collect_non_stream_body(response).await?;
            let events = parse_non_stream(&body)?;
            return Ok(Box::pin(futures_util::stream::iter(
                events.into_iter().map(Ok),
            )));
        }

        let mut byte_stream = response.bytes_stream();
        let stream = try_stream! {
            let mut sse = SseBuffer::new();
            let mut usage = Usage::default();
            let mut stop_reason = StopReason::EndTurn;
            let mut tools: BTreeMap<usize, ToolAccum> = BTreeMap::new();
            let mut done = false;

            'outer: while let Some(chunk) = byte_stream.next().await {
                let chunk = chunk.context("error reading OpenAI-compatible stream")?;
                for event in sse.push(&chunk) {
                    if event.data.trim() == "[DONE]" {
                        done = true;
                        break 'outer;
                    }
                    let data: Value = match serde_json::from_str(&event.data) {
                        Ok(value) => value,
                        Err(error) => {
                            Err(anyhow::anyhow!("invalid provider event JSON: {error}"))?;
                            continue;
                        }
                    };
                    if let Some(error) = data.get("error") {
                        let message = error
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown streaming error");
                        Err(anyhow::anyhow!("provider error: {message}"))?;
                    }
                    refresh_usage(&mut usage, &data);

                    let Some(choice) = data
                        .get("choices")
                        .and_then(Value::as_array)
                        .and_then(|choices| choices.first())
                    else {
                        continue;
                    };
                    if let Some(delta) = choice.get("delta") {
                        if let Some(text) = delta.get("content").and_then(Value::as_str)
                            && !text.is_empty()
                        {
                            yield StreamEvent::TextDelta(text.to_string());
                        }
                        if let Some(reasoning) = delta
                            .get("reasoning_content")
                            .or_else(|| delta.get("reasoning"))
                            .and_then(Value::as_str)
                            && !reasoning.is_empty()
                        {
                            yield StreamEvent::ThinkingDelta(reasoning.to_string());
                        }
                        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                            for call in calls {
                                let index = call
                                    .get("index")
                                    .and_then(Value::as_u64)
                                    .unwrap_or(tools.len() as u64) as usize;
                                let entry = tools.entry(index).or_default();
                                if let Some(id) = call.get("id").and_then(Value::as_str)
                                    && !id.is_empty()
                                {
                                    entry.id = id.to_string();
                                }
                                if let Some(function) = call.get("function") {
                                    if let Some(name) = function.get("name").and_then(Value::as_str)
                                        && !name.is_empty()
                                    {
                                        entry.name = name.to_string();
                                    }
                                    if let Some(arguments) = function.get("arguments").and_then(Value::as_str) {
                                        entry.arguments.push_str(arguments);
                                    }
                                }
                            }
                        }
                    }
                    if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                        stop_reason = map_finish_reason(reason);
                    }
                }
            }

            for (index, tool) in tools {
                let name = if tool.name.is_empty() {
                    "unknown_tool".to_string()
                } else {
                    tool.name
                };
                let id = if tool.id.is_empty() {
                    format!("call_{index}")
                } else {
                    tool.id
                };
                yield StreamEvent::ToolUseStart { index, id, name };
                let arguments = if tool.arguments.trim().is_empty() {
                    "{}".to_string()
                } else {
                    tool.arguments
                };
                yield StreamEvent::ToolUseDelta { index, partial_json: arguments };
                yield StreamEvent::ToolUseEnd { index };
                if stop_reason == StopReason::EndTurn {
                    stop_reason = StopReason::ToolUse;
                }
            }

            if !done {
                let _ = sse.flush();
            }
            yield StreamEvent::Done { stop_reason, usage };
        };
        Ok(Box::pin(stream))
    }
}

#[derive(Default)]
struct ToolAccum {
    id: String,
    name: String,
    arguments: String,
}

fn is_reasoning_model(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    m.starts_with("o1")
        || m.starts_with("o3")
        || m.starts_with("o4")
        || m.starts_with("gpt-5")
        || m.contains("deepseek-reasoner")
        || m.contains("qwq")
}

fn supports_stream_options(provider: &str) -> bool {
    !matches!(provider, "ollama" | "lmstudio" | "vllm" | "llamacpp")
}

pub fn convert_messages(system: &str, messages: &[Message]) -> Vec<Value> {
    let mut out = Vec::new();
    if !system.is_empty() {
        out.push(json!({"role": "system", "content": system}));
    }
    for message in messages {
        match message.role {
            Role::System => {
                let text = message.text();
                if !text.is_empty() {
                    out.push(json!({"role": "system", "content": text}));
                }
            }
            Role::User => {
                let mut parts = Vec::new();
                let mut tool_results = Vec::new();
                for block in &message.content {
                    match block {
                        ContentBlock::Text { text } if !text.is_empty() => {
                            parts.push(json!({"type": "text", "text": text}));
                        }
                        ContentBlock::Image { media_type, data } => {
                            parts.push(json!({
                                "type": "image_url",
                                "image_url": {"url": format!("data:{media_type};base64,{data}")}
                            }));
                        }
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            ..
                        } => {
                            tool_results.push(json!({
                                "role": "tool",
                                "tool_call_id": tool_use_id,
                                "content": content
                            }));
                        }
                        _ => {}
                    }
                }
                if !parts.is_empty() {
                    let content = if parts.len() == 1
                        && parts[0].get("type").and_then(Value::as_str) == Some("text")
                    {
                        parts[0]["text"].clone()
                    } else {
                        Value::Array(parts)
                    };
                    out.push(json!({"role": "user", "content": content}));
                }
                out.extend(tool_results);
            }
            Role::Assistant => {
                let text = message
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let tool_calls: Vec<Value> = message
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::ToolUse { id, name, input } => Some(json!({
                            "id": id,
                            "type": "function",
                            "function": {"name": name, "arguments": input.to_string()}
                        })),
                        _ => None,
                    })
                    .collect();
                if !tool_calls.is_empty() {
                    out.push(json!({
                        "role": "assistant",
                        "content": if text.is_empty() { Value::Null } else { Value::String(text) },
                        "tool_calls": tool_calls
                    }));
                } else if !text.is_empty() {
                    out.push(json!({"role": "assistant", "content": text}));
                }
            }
        }
    }
    out
}

fn map_finish_reason(reason: &str) -> StopReason {
    match reason {
        "stop" => StopReason::EndTurn,
        "tool_calls" | "function_call" => StopReason::ToolUse,
        "length" => StopReason::MaxTokens,
        "content_filter" => StopReason::ContentFilter,
        other => StopReason::Unknown(other.to_string()),
    }
}

fn parse_non_stream(body: &Value) -> Result<Vec<StreamEvent>> {
    let mut events = Vec::new();
    let mut usage = Usage::default();
    refresh_usage(&mut usage, body);
    let choice = body
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .context("provider response had no choices")?;
    let message = choice.get("message").cloned().unwrap_or(Value::Null);
    if let Some(text) = message.get("content").and_then(Value::as_str)
        && !text.is_empty()
    {
        events.push(StreamEvent::TextDelta(text.to_string()));
    }
    if let Some(reasoning) = message
        .get("reasoning_content")
        .or_else(|| message.get("reasoning"))
        .and_then(Value::as_str)
        && !reasoning.is_empty()
    {
        events.push(StreamEvent::ThinkingDelta(reasoning.to_string()));
    }
    if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
        for (index, call) in calls.iter().enumerate() {
            let id = call
                .get("id")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| format!("call_{index}"));
            let name = call
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("unknown_tool")
                .to_string();
            let args = call
                .get("function")
                .and_then(|f| f.get("arguments"))
                .and_then(Value::as_str)
                .unwrap_or("{}")
                .to_string();
            events.push(StreamEvent::ToolUseStart { index, id, name });
            events.push(StreamEvent::ToolUseDelta {
                index,
                partial_json: args,
            });
            events.push(StreamEvent::ToolUseEnd { index });
        }
    }
    let stop_reason = choice
        .get("finish_reason")
        .and_then(Value::as_str)
        .map(map_finish_reason)
        .unwrap_or_else(|| {
            if events
                .iter()
                .any(|event| matches!(event, StreamEvent::ToolUseStart { .. }))
            {
                StopReason::ToolUse
            } else {
                StopReason::EndTurn
            }
        });
    events.push(StreamEvent::Done { stop_reason, usage });
    Ok(events)
}

/// Extract a human-readable provider error without dumping headers/keys.
pub fn readable_error(body: &str) -> String {
    if let Ok(value) = serde_json::from_str::<Value>(body)
        && let Some(message) = value
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(Value::as_str)
    {
        return truncate_error(message);
    }
    truncate_error(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_tool_result_messages() {
        let messages = vec![
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "call_1".into(),
                    name: "Read".into(),
                    input: json!({"path": "x"}),
                }],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "call_1".into(),
                    content: "ok".into(),
                    is_error: false,
                }],
            },
        ];
        let converted = convert_messages("sys", &messages);
        assert_eq!(converted[0]["role"], "system");
        assert_eq!(converted[1]["role"], "assistant");
        assert!(converted[1].get("tool_calls").is_some());
        assert_eq!(converted[2]["role"], "tool");
    }

    #[test]
    fn image_content_uses_data_url() {
        let messages = vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Image {
                media_type: "image/png".into(),
                data: "AAA".into(),
            }],
        }];
        let converted = convert_messages("", &messages);
        assert_eq!(converted[0]["content"][0]["type"], "image_url");
        assert!(
            converted[0]["content"][0]["image_url"]["url"]
                .as_str()
                .unwrap()
                .starts_with("data:image/png;base64,")
        );
    }
}
