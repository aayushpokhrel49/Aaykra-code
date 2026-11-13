//! Anthropic Messages API provider (native tool use, thinking, vision).

use std::collections::HashSet;

use anyhow::{Context, Result};
use async_stream::try_stream;
use futures_util::StreamExt;
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use serde_json::{Value, json};

use crate::config::{Effort, Protocol, ProviderConfig};
use crate::providers::{
    ChatRequest, ChatStream, Provider, SseBuffer, StopReason, StreamEvent, collect_non_stream_body,
    merge_extra_body, post_json, refresh_usage, response_is_sse,
};
use crate::types::{ContentBlock, Message, Role, Usage};

pub struct AnthropicProvider {
    client: reqwest::Client,
    provider_name: String,
    model: String,
    config: ProviderConfig,
    api_key: Option<String>,
}

impl AnthropicProvider {
    pub fn new(
        client: reqwest::Client,
        provider_name: String,
        model: String,
        config: ProviderConfig,
        api_key: Option<String>,
    ) -> Result<Self> {
        Ok(Self {
            client,
            provider_name,
            model,
            config,
            api_key,
        })
    }

    fn endpoint(&self) -> String {
        let base = self.config.base_url.trim_end_matches('/');
        if base.ends_with("/messages") {
            base.to_string()
        } else if base.ends_with("/v1") {
            format!("{base}/messages")
        } else {
            format!("{base}/v1/messages")
        }
    }

    fn headers(&self) -> Result<HeaderMap> {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
        headers.insert(
            HeaderName::from_static("anthropic-version"),
            HeaderValue::from_static("2023-06-01"),
        );
        if let Some(key) = &self.api_key {
            headers.insert(
                HeaderName::from_static("x-api-key"),
                HeaderValue::from_str(key).context("API key is not a valid header value")?,
            );
        }
        for (name, value) in &self.config.headers {
            headers.insert(
                HeaderName::from_bytes(name.as_bytes()).context("invalid custom header")?,
                HeaderValue::from_str(value).context("invalid custom header value")?,
            );
        }
        Ok(headers)
    }
}

#[async_trait::async_trait]
impl Provider for AnthropicProvider {
    fn provider_name(&self) -> &str {
        &self.provider_name
    }

    fn model_name(&self) -> &str {
        &self.model
    }

    fn protocol(&self) -> Protocol {
        Protocol::Anthropic
    }

    async fn stream_chat(&self, request: ChatRequest) -> Result<ChatStream> {
        let mut body = json!({
            "model": self.model,
            "max_tokens": request.max_tokens,
            "system": [{
                "type": "text",
                "text": request.system,
                "cache_control": {"type": "ephemeral"}
            }],
            "messages": convert_messages(&request.messages),
            "stream": true
        });

        if request.effort != Effort::Off {
            let budget = request.effort.anthropic_budget();
            let needed = budget + 2_048;
            if body["max_tokens"].as_u64().unwrap_or(0) < u64::from(needed) {
                body["max_tokens"] = json!(needed);
            }
            body["thinking"] = json!({
                "type": "enabled",
                "budget_tokens": budget
            });
        } else if let Some(temperature) = request.temperature {
            body["temperature"] = json!(temperature.clamp(0.0, 1.0));
        }

        if !request.tools.is_empty() {
            let mut tools: Vec<Value> = request
                .tools
                .iter()
                .map(|tool| {
                    json!({
                        "name": tool.name,
                        "description": tool.description,
                        "input_schema": tool.input_schema
                    })
                })
                .collect();
            if let Some(last) = tools.last_mut() {
                last["cache_control"] = json!({"type": "ephemeral"});
            }
            body["tools"] = Value::Array(tools);
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
            let mut done = false;
            let mut open_tools: HashSet<usize> = HashSet::new();

            'outer: while let Some(chunk) = byte_stream.next().await {
                let chunk = chunk.context("error reading Anthropic stream")?;
                for event in sse.push(&chunk) {
                    if event.data.trim() == "[DONE]" { continue; }
                    let data: Value = match serde_json::from_str(&event.data) {
                        Ok(value) => value,
                        Err(error) => {
                            yield Err(anyhow::anyhow!("invalid Anthropic event JSON: {error}"))?;
                            continue;
                        }
                    };
                    let kind = data
                        .get("type")
                        .and_then(Value::as_str)
                        .or(event.event.as_deref())
                        .unwrap_or_default();

                    match kind {
                        "message_start" => {
                            if let Some(message) = data.get("message") {
                                refresh_usage(&mut usage, message);
                                refresh_usage(&mut usage, &json!({"usage": message.get("usage").cloned().unwrap_or(Value::Null)}));
                            }
                        }
                        "content_block_start" => {
                            let index = data.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                            let block = data.get("content_block").cloned().unwrap_or(Value::Null);
                            match block.get("type").and_then(Value::as_str) {
                                Some("tool_use") => {
                                    let id = block.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
                                    let name = block.get("name").and_then(Value::as_str).unwrap_or_default().to_string();
                                    open_tools.insert(index);
                                    yield StreamEvent::ToolUseStart { index, id, name };
                                }
                                Some("text") => {}
                                Some("thinking") => {}
                                _ => {}
                            }
                        }
                        "content_block_delta" => {
                            let index = data.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                            let delta = data.get("delta").cloned().unwrap_or(Value::Null);
                            match delta.get("type").and_then(Value::as_str) {
                                Some("text_delta") => {
                                    if let Some(text) = delta.get("text").and_then(Value::as_str) {
                                        yield StreamEvent::TextDelta(text.to_string());
                                    }
                                }
                                Some("thinking_delta") => {
                                    if let Some(text) = delta.get("thinking").and_then(Value::as_str) {
                                        yield StreamEvent::ThinkingDelta(text.to_string());
                                    }
                                }
                                Some("signature_delta") => {
                                    if let Some(signature) = delta.get("signature").and_then(Value::as_str) {
                                        yield StreamEvent::ThinkingSignature(signature.to_string());
                                    }
                                }
                                Some("input_json_delta") => {
                                    if let Some(partial) = delta.get("partial_json").and_then(Value::as_str) {
                                        yield StreamEvent::ToolUseDelta { index, partial_json: partial.to_string() };
                                    }
                                }
                                _ => {}
                            }
                        }
                        "content_block_stop" => {
                            let index = data.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                            if open_tools.remove(&index) {
                                yield StreamEvent::ToolUseEnd { index };
                            }
                        }
                        "message_delta" => {
                            refresh_usage(&mut usage, &data);
                            if let Some(reason) = data
                                .get("delta")
                                .and_then(|d| d.get("stop_reason"))
                                .and_then(Value::as_str)
                            {
                                stop_reason = map_stop_reason(reason);
                            }
                        }
                        "message_stop" => {
                            done = true;
                            break 'outer;
                        }
                        "error" => {
                            let message = data
                                .get("error")
                                .and_then(|e| e.get("message"))
                                .and_then(Value::as_str)
                                .unwrap_or("unknown Anthropic streaming error");
                            Err(anyhow::anyhow!("Anthropic error: {message}"))?;
                        }
                        "ping" => {}
                        _ => {}
                    }
                }
            }

            for index in open_tools {
                yield StreamEvent::ToolUseEnd { index };
            }
            if !done {
                let event = sse.flush();
                // We do not replay events from EOF; this branch is mostly a
                // safety net for proxies that omit message_stop.
                let _ = event;
            }
            yield StreamEvent::Done { stop_reason, usage };
        };
        Ok(Box::pin(stream))
    }
}

pub fn convert_messages(messages: &[Message]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    for message in messages {
        if message.role == Role::System {
            continue;
        }
        let role = if message.role == Role::Assistant {
            "assistant"
        } else {
            "user"
        };
        let content: Vec<Value> = message.content.iter().filter_map(convert_block).collect();
        if content.is_empty() {
            continue;
        }
        if let Some(last) = out.last_mut()
            && last.get("role").and_then(Value::as_str) == Some(role)
        {
            if let Some(existing) = last.get_mut("content").and_then(Value::as_array_mut) {
                existing.extend(content);
            }
        } else {
            out.push(json!({"role": role, "content": content}));
        }
    }
    out
}

fn convert_block(block: &ContentBlock) -> Option<Value> {
    match block {
        ContentBlock::Text { text } if !text.is_empty() => {
            Some(json!({"type": "text", "text": text}))
        }
        ContentBlock::Text { .. } => None,
        ContentBlock::Thinking {
            thinking,
            signature: Some(signature),
        } => Some(json!({
            "type": "thinking",
            "thinking": thinking,
            "signature": signature
        })),
        ContentBlock::Thinking { .. } => None,
        ContentBlock::Image { media_type, data } => Some(json!({
            "type": "image",
            "source": {
                "type": "base64",
                "media_type": media_type,
                "data": data
            }
        })),
        ContentBlock::ToolUse { id, name, input } => Some(json!({
            "type": "tool_use",
            "id": id,
            "name": name,
            "input": input
        })),
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => Some(json!({
            "type": "tool_result",
            "tool_use_id": tool_use_id,
            "content": content,
            "is_error": is_error
        })),
    }
}

fn map_stop_reason(reason: &str) -> StopReason {
    match reason {
        "end_turn" | "stop_sequence" if reason == "end_turn" => StopReason::EndTurn,
        "tool_use" => StopReason::ToolUse,
        "max_tokens" => StopReason::MaxTokens,
        "stop_sequence" => StopReason::StopSequence,
        "refusal" => StopReason::Refusal,
        other => StopReason::Unknown(other.to_string()),
    }
}

fn parse_non_stream(body: &Value) -> Result<Vec<StreamEvent>> {
    let mut events = Vec::new();
    let mut usage = Usage::default();
    refresh_usage(&mut usage, body);
    if let Some(blocks) = body.get("content").and_then(Value::as_array) {
        for (index, block) in blocks.iter().enumerate() {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(text) = block.get("text").and_then(Value::as_str) {
                        events.push(StreamEvent::TextDelta(text.to_string()));
                    }
                }
                Some("thinking") => {
                    if let Some(text) = block.get("thinking").and_then(Value::as_str) {
                        events.push(StreamEvent::ThinkingDelta(text.to_string()));
                    }
                    if let Some(signature) = block.get("signature").and_then(Value::as_str) {
                        events.push(StreamEvent::ThinkingSignature(signature.to_string()));
                    }
                }
                Some("tool_use") => {
                    let id = block
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let name = block
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    events.push(StreamEvent::ToolUseStart { index, id, name });
                    let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
                    events.push(StreamEvent::ToolUseDelta {
                        index,
                        partial_json: input.to_string(),
                    });
                    events.push(StreamEvent::ToolUseEnd { index });
                }
                _ => {}
            }
        }
    }
    let stop_reason = body
        .get("stop_reason")
        .and_then(Value::as_str)
        .map(map_stop_reason)
        .unwrap_or(StopReason::EndTurn);
    events.push(StreamEvent::Done { stop_reason, usage });
    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_results_are_user_blocks() {
        let messages = vec![
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "toolu_1".into(),
                    name: "Read".into(),
                    input: json!({"path":"src/lib.rs"}),
                }],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "toolu_1".into(),
                    content: "hello".into(),
                    is_error: false,
                }],
            },
        ];
        let converted = convert_messages(&messages);
        assert_eq!(converted[0]["role"], "assistant");
        assert_eq!(converted[1]["role"], "user");
        assert_eq!(converted[1]["content"][0]["type"], "tool_result");
    }

    #[test]
    fn skip_unsigned_thinking() {
        let messages = vec![Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Thinking {
                    thinking: "hidden".into(),
                    signature: None,
                },
                ContentBlock::text("visible"),
            ],
        }];
        let converted = convert_messages(&messages);
        assert_eq!(converted[0]["content"].as_array().unwrap().len(), 1);
    }
}
