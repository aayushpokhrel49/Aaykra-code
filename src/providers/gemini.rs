//! Google Gemini `generateContent` streaming provider.

use std::collections::HashMap;

use anyhow::{Context, Result};
use async_stream::try_stream;
use futures_util::StreamExt;
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use serde_json::{Map, Value, json};

use crate::config::{Effort, Protocol, ProviderConfig};
use crate::providers::{
    ChatRequest, ChatStream, Provider, SseBuffer, StopReason, StreamEvent, collect_non_stream_body,
    merge_extra_body, post_json, response_is_sse, truncate_error,
};
use crate::types::{ContentBlock, Message, Role, Usage};

pub struct GeminiProvider {
    client: reqwest::Client,
    provider_name: String,
    model: String,
    config: ProviderConfig,
    api_key: Option<String>,
}

impl GeminiProvider {
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
        if base.contains(":streamGenerateContent") {
            return base.to_string();
        }
        let model = self.model.trim_start_matches("models/");
        format!("{base}/models/{model}:streamGenerateContent?alt=sse")
    }

    fn headers(&self) -> Result<HeaderMap> {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
        if let Some(key) = &self.api_key {
            headers.insert(
                HeaderName::from_static("x-goog-api-key"),
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

    fn thinking_budget(effort: Effort) -> i64 {
        match effort {
            Effort::Off => 0,
            Effort::Low => 1_024,
            Effort::Medium => 8_192,
            Effort::High => 24_576,
            Effort::Max => 32_768,
        }
    }
}

#[async_trait::async_trait]
impl Provider for GeminiProvider {
    fn provider_name(&self) -> &str {
        &self.provider_name
    }

    fn model_name(&self) -> &str {
        &self.model
    }

    fn protocol(&self) -> Protocol {
        Protocol::Gemini
    }

    async fn stream_chat(&self, request: ChatRequest) -> Result<ChatStream> {
        let mut body = json!({
            "contents": convert_messages(&request.messages),
            "generationConfig": {
                "maxOutputTokens": request.max_tokens
            }
        });
        if !request.system.is_empty() {
            body["systemInstruction"] = json!({
                "parts": [{"text": request.system}]
            });
        }
        if let Some(temperature) = request.temperature {
            body["generationConfig"]["temperature"] = json!(temperature);
        }
        if self.model.to_ascii_lowercase().contains("gemini-2.5") && request.effort != Effort::Off {
            body["generationConfig"]["thinkingConfig"] =
                json!({"thinkingBudget": Self::thinking_budget(request.effort)});
        }
        if !request.tools.is_empty() {
            body["tools"] = json!([{
                "functionDeclarations": request.tools.iter().map(|tool| {
                    json!({
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": sanitize_schema(&tool.input_schema)
                    })
                }).collect::<Vec<_>>()
            }]);
        }
        merge_extra_body(&self.config, &mut body);

        let response = post_json(&self.client, &self.endpoint(), self.headers()?, &body).await?;
        if !response_is_sse(&response) {
            let body = collect_non_stream_body(response).await?;
            let events = parse_response(&body)?;
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
            let mut tool_index = 0usize;

            'outer: while let Some(chunk) = byte_stream.next().await {
                let chunk = chunk.context("error reading Gemini stream")?;
                for event in sse.push(&chunk) {
                    let data: Value = match serde_json::from_str(&event.data) {
                        Ok(value) => value,
                        Err(error) => {
                            Err(anyhow::anyhow!("invalid Gemini event JSON: {error}"))?;
                            continue;
                        }
                    };
                    if let Some(error) = data.get("error") {
                        let message = error.get("message").and_then(Value::as_str)
                            .unwrap_or("unknown Gemini streaming error");
                        Err(anyhow::anyhow!("Gemini error: {message}"))?;
                    }
                    let chunk_events = parse_chunk(&data, &mut usage, &mut tool_index)?;
                    for event in chunk_events {
                        match &event {
                            StreamEvent::TextDelta(_) | StreamEvent::ThinkingDelta(_) => yield event,
                            _ => yield event,
                        }
                    }
                    if let Some(reason) = data
                        .get("candidates")
                        .and_then(Value::as_array)
                        .and_then(|c| c.first())
                        .and_then(|c| c.get("finishReason"))
                        .and_then(Value::as_str)
                    {
                        stop_reason = map_finish_reason(reason);
                        done = true;
                        break 'outer;
                    }
                }
            }
            let _ = done;
            let _ = sse.flush();
            if stop_reason == StopReason::EndTurn && tool_index > 0 {
                stop_reason = StopReason::ToolUse;
            }
            yield StreamEvent::Done { stop_reason, usage };
        };
        Ok(Box::pin(stream))
    }
}

fn parse_chunk(
    data: &Value,
    usage: &mut Usage,
    tool_index: &mut usize,
) -> Result<Vec<StreamEvent>> {
    let mut events = Vec::new();
    if let Some(metadata) = data.get("usageMetadata") {
        if let Some(v) = metadata.get("promptTokenCount").and_then(Value::as_u64) {
            usage.input_tokens = usage.input_tokens.max(v);
        }
        if let Some(v) = metadata.get("candidatesTokenCount").and_then(Value::as_u64) {
            usage.output_tokens = usage.output_tokens.max(v);
        }
        if let Some(v) = metadata
            .get("cachedContentTokenCount")
            .and_then(Value::as_u64)
        {
            usage.cache_read_input_tokens = usage.cache_read_input_tokens.max(v);
        }
        if let Some(v) = metadata.get("thoughtsTokenCount").and_then(Value::as_u64) {
            usage.reasoning_tokens = usage.reasoning_tokens.max(v);
        }
    }
    let Some(parts) = data
        .get("candidates")
        .and_then(Value::as_array)
        .and_then(|c| c.first())
        .and_then(|c| c.get("content"))
        .and_then(|c| c.get("parts"))
        .and_then(Value::as_array)
    else {
        return Ok(events);
    };
    for part in parts {
        if let Some(text) = part.get("text").and_then(Value::as_str)
            && !text.is_empty()
        {
            if part
                .get("thought")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                events.push(StreamEvent::ThinkingDelta(text.to_string()));
                if let Some(signature) = part.get("thoughtSignature").and_then(Value::as_str) {
                    events.push(StreamEvent::ThinkingSignature(signature.to_string()));
                }
            } else {
                events.push(StreamEvent::TextDelta(text.to_string()));
            }
        }
        if let Some(call) = part.get("functionCall") {
            let name = call
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("unknown_tool")
                .to_string();
            let args = call.get("args").cloned().unwrap_or_else(|| json!({}));
            let index = *tool_index;
            *tool_index += 1;
            let id = format!("gemini_call_{index}");
            events.push(StreamEvent::ToolUseStart { index, id, name });
            events.push(StreamEvent::ToolUseDelta {
                index,
                partial_json: args.to_string(),
            });
            events.push(StreamEvent::ToolUseEnd { index });
        }
    }
    Ok(events)
}

fn map_finish_reason(reason: &str) -> StopReason {
    match reason {
        "STOP" => StopReason::EndTurn,
        "MAX_TOKENS" => StopReason::MaxTokens,
        "SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "SPII" => {
            StopReason::ContentFilter
        }
        other => StopReason::Unknown(other.to_string()),
    }
}

fn parse_response(body: &Value) -> Result<Vec<StreamEvent>> {
    let chunks: Vec<&Value> = if let Some(array) = body.as_array() {
        array.iter().collect()
    } else {
        vec![body]
    };
    let mut events = Vec::new();
    let mut usage = Usage::default();
    let mut stop_reason = StopReason::EndTurn;
    let mut tool_index = 0;
    for chunk in chunks {
        if let Some(error) = chunk.get("error") {
            anyhow::bail!(
                "Gemini error: {}",
                error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error")
            );
        }
        events.extend(parse_chunk(chunk, &mut usage, &mut tool_index)?);
        if let Some(reason) = chunk
            .get("candidates")
            .and_then(Value::as_array)
            .and_then(|c| c.first())
            .and_then(|c| c.get("finishReason"))
            .and_then(Value::as_str)
        {
            stop_reason = map_finish_reason(reason);
        }
    }
    if stop_reason == StopReason::EndTurn && tool_index > 0 {
        stop_reason = StopReason::ToolUse;
    }
    events.push(StreamEvent::Done { stop_reason, usage });
    Ok(events)
}

pub fn convert_messages(messages: &[Message]) -> Vec<Value> {
    let mut tool_names = HashMap::new();
    for message in messages {
        for block in &message.content {
            if let ContentBlock::ToolUse { id, name, .. } = block {
                tool_names.insert(id.clone(), name.clone());
            }
        }
    }

    let mut out: Vec<Value> = Vec::new();
    for message in messages {
        if message.role == Role::System {
            continue;
        }
        let role = if message.role == Role::Assistant {
            "model"
        } else {
            "user"
        };
        let mut parts = Vec::new();
        for block in &message.content {
            match block {
                ContentBlock::Text { text } if !text.is_empty() => {
                    parts.push(json!({"text": text}));
                }
                ContentBlock::Image { media_type, data } => {
                    parts.push(json!({
                        "inlineData": {"mimeType": media_type, "data": data}
                    }));
                }
                ContentBlock::ToolUse { name, input, .. } => {
                    parts.push(json!({
                        "functionCall": {"name": name, "args": input}
                    }));
                }
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } => {
                    let name = tool_names
                        .get(tool_use_id)
                        .cloned()
                        .unwrap_or_else(|| tool_use_id.clone());
                    parts.push(json!({
                        "functionResponse": {
                            "name": name,
                            "response": {"output": content, "is_error": is_error}
                        }
                    }));
                }
                _ => {}
            }
        }
        if parts.is_empty() {
            continue;
        }
        if let Some(last) = out.last_mut()
            && last.get("role").and_then(Value::as_str) == Some(role)
        {
            if let Some(existing) = last.get_mut("parts").and_then(Value::as_array_mut) {
                existing.extend(parts);
            }
        } else {
            out.push(json!({"role": role, "parts": parts}));
        }
    }
    out
}

fn sanitize_schema(value: &Value) -> Value {
    let allowed = [
        "type",
        "format",
        "description",
        "nullable",
        "enum",
        "items",
        "properties",
        "required",
        "minimum",
        "maximum",
        "minItems",
        "maxItems",
        "minLength",
        "maxLength",
        "pattern",
        "anyOf",
        "oneOf",
        "default",
    ];
    match value {
        Value::Object(map) => {
            let mut result = Map::new();
            for (key, child) in map {
                if allowed.contains(&key.as_str()) {
                    match key.as_str() {
                        "properties" => {
                            if let Some(props) = child.as_object() {
                                let converted: Map<String, Value> = props
                                    .iter()
                                    .map(|(name, schema)| (name.clone(), sanitize_schema(schema)))
                                    .collect();
                                result.insert(key.clone(), Value::Object(converted));
                            }
                        }
                        "items" | "anyOf" | "oneOf" => {
                            if let Some(items) = child.as_array() {
                                result.insert(
                                    key.clone(),
                                    Value::Array(items.iter().map(sanitize_schema).collect()),
                                );
                            } else {
                                result.insert(key.clone(), sanitize_schema(child));
                            }
                        }
                        _ => {
                            result.insert(key.clone(), child.clone());
                        }
                    }
                }
            }
            Value::Object(result)
        }
        other => other.clone(),
    }
}

fn _format_error(body: &str) -> String {
    truncate_error(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_results_reference_function_name() {
        let messages = vec![
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "gemini_call_0".into(),
                    name: "Read".into(),
                    input: json!({"path":"x"}),
                }],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "gemini_call_0".into(),
                    content: "ok".into(),
                    is_error: false,
                }],
            },
        ];
        let converted = convert_messages(&messages);
        assert_eq!(converted[1]["parts"][0]["functionResponse"]["name"], "Read");
    }

    #[test]
    fn schema_strips_additional_properties() {
        let schema = json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "path": {"type": "string", "additionalProperties": false}
            }
        });
        let sanitized = sanitize_schema(&schema);
        assert!(sanitized.get("additionalProperties").is_none());
    }

    #[test]
    fn merges_same_roles() {
        let messages = vec![Message::user_text("one"), Message::user_text("two")];
        let converted = convert_messages(&messages);
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0]["parts"].as_array().unwrap().len(), 2);
    }
}
