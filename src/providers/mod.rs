//! Provider-neutral streaming chat interface and shared HTTP/SSE plumbing.

pub mod anthropic;
pub mod gemini;
pub mod openai;

use std::collections::BTreeMap;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures_util::Stream;
use futures_util::StreamExt;
use indexmap::IndexMap;
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue, USER_AGENT};
use serde_json::Value;

use crate::auth::{Credentials, require_api_key, resolve_api_key};
use crate::config::{Config, Effort, Protocol, ProviderConfig};
use crate::models::{ModelRef, ResolvedModel};
use crate::paths::AaykraPaths;
use crate::types::{Message, ToolDefinition, Usage};

pub type ChatStream = Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>;

#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub model: String,
    pub system: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
    pub max_tokens: u32,
    pub temperature: Option<f32>,
    pub effort: Effort,
    /// Mostly informational; all current implementations stream.
    pub stream: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    StopSequence,
    Refusal,
    ContentFilter,
    Unknown(String),
}

impl StopReason {
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::EndTurn => "end_turn".into(),
            Self::ToolUse => "tool_use".into(),
            Self::MaxTokens => "max_tokens".into(),
            Self::StopSequence => "stop_sequence".into(),
            Self::Refusal => "refusal".into(),
            Self::ContentFilter => "content_filter".into(),
            Self::Unknown(value) => value.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum StreamEvent {
    TextDelta(String),
    ThinkingDelta(String),
    ThinkingSignature(String),
    ToolUseStart {
        index: usize,
        id: String,
        name: String,
    },
    ToolUseDelta {
        index: usize,
        partial_json: String,
    },
    ToolUseEnd {
        index: usize,
    },
    Done {
        stop_reason: StopReason,
        usage: Usage,
    },
}

#[async_trait::async_trait]
pub trait Provider: Send + Sync {
    fn provider_name(&self) -> &str;
    fn model_name(&self) -> &str;
    fn protocol(&self) -> Protocol;
    async fn stream_chat(&self, request: ChatRequest) -> Result<ChatStream>;
}

/// Shared provider construction and HTTP client.
pub struct ProviderRuntime {
    config: Arc<Config>,
    paths: AaykraPaths,
    client: reqwest::Client,
}

impl ProviderRuntime {
    pub fn new(config: Arc<Config>, paths: AaykraPaths) -> Result<Self> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(20))
            .pool_idle_timeout(Duration::from_secs(90))
            .user_agent(concat!("AaykraCode/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("could not initialize HTTP client")?;
        Ok(Self {
            config,
            paths,
            client,
        })
    }

    #[must_use]
    pub fn config(&self) -> &Config {
        &self.config
    }

    #[must_use]
    pub fn http(&self) -> &reqwest::Client {
        &self.client
    }

    pub fn build(&self, resolved: &ResolvedModel) -> Result<Arc<dyn Provider>> {
        let name = &resolved.reference.provider;
        let api_key = require_api_key(&self.config, name, &resolved.provider, &self.paths)?;
        let provider: Arc<dyn Provider> = match resolved.provider.protocol {
            Protocol::Anthropic => Arc::new(anthropic::AnthropicProvider::new(
                self.client.clone(),
                name.clone(),
                resolved.reference.model.clone(),
                resolved.provider.clone(),
                api_key,
            )?),
            Protocol::OpenAi => Arc::new(openai::OpenAiProvider::new(
                self.client.clone(),
                name.clone(),
                resolved.reference.model.clone(),
                resolved.provider.clone(),
                api_key,
            )?),
            Protocol::Gemini => Arc::new(gemini::GeminiProvider::new(
                self.client.clone(),
                name.clone(),
                resolved.reference.model.clone(),
                resolved.provider.clone(),
                api_key,
            )?),
        };
        Ok(provider)
    }

    #[must_use]
    pub fn has_key(&self, provider_name: &str) -> bool {
        self.config
            .providers
            .get(provider_name)
            .is_some_and(|provider| {
                resolve_api_key(&self.config, provider_name, provider, &self.paths)
                    .ok()
                    .flatten()
                    .is_some()
                    || !provider.requires_key
            })
    }

    /// Ask an OpenAI-compatible or Anthropic endpoint for available models.
    /// Used by `/model` when explicitly requested; gracefully returns an error
    /// for providers without a models endpoint.
    pub async fn list_remote_models(&self, provider_name: &str) -> Result<Vec<String>> {
        let provider = self
            .config
            .provider(provider_name)
            .with_context(|| format!("unknown provider `{provider_name}`"))?;
        if !matches!(provider.protocol, Protocol::OpenAi) {
            return Ok(provider.known_models.clone());
        }
        let key = resolve_api_key(&self.config, provider_name, provider, &self.paths)?;
        let url = join_url(&provider.base_url, "models");
        let mut request = self
            .client
            .get(url)
            .headers(headers(provider, key.as_deref())?);
        if let Some(extra) = provider.extra_body.get("models_query") {
            request = request.query(&extra);
        }
        let response = request.send().await.context("could not list models")?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("provider returned {status}: {}", truncate_error(&text));
        }
        let body: Value = serde_json::from_str(&text).context("invalid model list JSON")?;
        let models = body
            .get("data")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.get("id").and_then(Value::as_str))
                    .map(ToOwned::to_owned)
                    .collect()
            })
            .or_else(|| {
                body.get("models").and_then(Value::as_array).map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.get("name").and_then(Value::as_str))
                        .map(ToOwned::to_owned)
                        .collect()
                })
            })
            .unwrap_or_default();
        Ok(models)
    }
}

pub fn join_url(base: &str, path: &str) -> String {
    let base = base.trim_end_matches('/');
    let path = path.trim_start_matches('/');
    if base.ends_with(path) {
        base.to_string()
    } else {
        format!("{base}/{path}")
    }
}

pub fn headers(provider: &ProviderConfig, api_key: Option<&str>) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
    headers.insert(
        USER_AGENT,
        HeaderValue::from_static(concat!(
            "AaykraCode/",
            env!("CARGO_PKG_VERSION"),
            " (native Rust coding agent)"
        )),
    );
    if let Some(key) = api_key {
        let value = HeaderValue::from_str(&format!("Bearer {key}"))
            .context("API key contains characters invalid for an HTTP header")?;
        headers.insert(reqwest::header::AUTHORIZATION, value);
    }
    for (name, value) in &provider.headers {
        let name = HeaderName::from_bytes(name.as_bytes())
            .with_context(|| format!("invalid header name `{name}`"))?;
        let value = HeaderValue::from_str(value)
            .with_context(|| format!("invalid value for header `{name}`"))?;
        headers.insert(name, value);
    }
    Ok(headers)
}

pub fn merge_extra_body(provider: &ProviderConfig, body: &mut Value) {
    if provider.extra_body.is_empty() {
        return;
    }
    let extras: BTreeMap<String, Value> = provider.extra_body.clone().into_iter().collect();
    if let Some(map) = body.as_object_mut() {
        for (k, v) in extras {
            map.insert(k, v);
        }
    }
}

pub async fn post_json(
    client: &reqwest::Client,
    url: &str,
    headers: HeaderMap,
    body: &Value,
) -> Result<reqwest::Response> {
    let response = client
        .post(url)
        .headers(headers)
        .json(body)
        .send()
        .await
        .context("provider request failed")?;
    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        bail!("provider returned HTTP {status}: {}", truncate_error(&text));
    }
    Ok(response)
}

pub fn response_is_sse(response: &reqwest::Response) -> bool {
    response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("text/event-stream"))
}

pub async fn collect_non_stream_body(response: reqwest::Response) -> Result<Value> {
    let text = response.text().await.context("could not read response")?;
    serde_json::from_str(&text)
        .with_context(|| format!("invalid JSON response: {}", truncate_error(&text)))
}

pub fn truncate_error(text: &str) -> String {
    let clean = text.trim();
    let mut result: String = clean.chars().take(2_000).collect();
    if clean.chars().count() > 2_000 {
        result.push_str("…");
    }
    if result.is_empty() {
        "<empty body>".into()
    } else {
        result
    }
}

pub fn refresh_usage(usage: &mut Usage, body: &Value) {
    if let Some(u) = body.get("usage") {
        let input = u
            .get("input_tokens")
            .or_else(|| u.get("prompt_tokens"))
            .and_then(Value::as_u64);
        let output = u
            .get("output_tokens")
            .or_else(|| u.get("completion_tokens"))
            .and_then(Value::as_u64);
        if let Some(v) = input {
            usage.input_tokens = usage.input_tokens.max(v);
        }
        if let Some(v) = output {
            usage.output_tokens = usage.output_tokens.max(v);
        }
        if let Some(v) = u
            .get("cache_read_input_tokens")
            .or_else(|| {
                u.get("prompt_tokens_details")
                    .and_then(|d| d.get("cached_tokens"))
            })
            .or_else(|| u.get("cache_read_tokens"))
            .and_then(Value::as_u64)
        {
            usage.cache_read_input_tokens = usage.cache_read_input_tokens.max(v);
        }
        if let Some(v) = u
            .get("cache_creation_input_tokens")
            .or_else(|| u.get("cache_write_tokens"))
            .and_then(Value::as_u64)
        {
            usage.cache_creation_input_tokens = usage.cache_creation_input_tokens.max(v);
        }
        if let Some(v) = u
            .get("completion_tokens_details")
            .and_then(|d| d.get("reasoning_tokens"))
            .or_else(|| u.get("reasoning_tokens"))
            .and_then(Value::as_u64)
        {
            usage.reasoning_tokens = usage.reasoning_tokens.max(v);
        }
    }
}

/// A small, allocation-light SSE parser that tolerates chunks split at any byte
/// boundary and CRLF line endings.
#[derive(Debug, Default)]
pub struct SseBuffer {
    buffer: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

impl SseBuffer {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.buffer
            .extend(chunk.iter().copied().filter(|&b| b != b'\r'));
        let mut events = Vec::new();
        while let Some(position) = find_subslice(&self.buffer, b"\n\n") {
            let block: Vec<u8> = self.buffer.drain(..position + 2).collect();
            if let Some(event) = parse_sse_block(&block[..position]) {
                events.push(event);
            }
        }
        events
    }

    pub fn flush(&mut self) -> Vec<SseEvent> {
        if self.buffer.is_empty() {
            return Vec::new();
        }
        let block = std::mem::take(&mut self.buffer);
        parse_sse_block(&block).into_iter().collect()
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn parse_sse_block(block: &[u8]) -> Option<SseEvent> {
    let text = String::from_utf8_lossy(block);
    let mut event = None;
    let mut data = Vec::new();
    for line in text.lines() {
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        if let Some(value) = line.strip_prefix("event:") {
            event = Some(value.trim().to_string());
        } else if let Some(value) = line.strip_prefix("data:") {
            data.push(value.trim_start().to_string());
        }
    }
    if data.is_empty() {
        None
    } else {
        Some(SseEvent {
            event,
            data: data.join("\n"),
        })
    }
}

/// Accumulate a stream into text. Used for compaction/titles; tool calls are
/// ignored because those requests never expose tools.
pub async fn collect_text(provider: &dyn Provider, request: ChatRequest) -> Result<String> {
    let mut stream = provider.stream_chat(request).await?;
    let mut out = String::new();
    while let Some(event) = stream.next().await {
        match event? {
            StreamEvent::TextDelta(text) => out.push_str(&text),
            StreamEvent::Done { .. } => break,
            _ => {}
        }
    }
    Ok(out)
}

#[must_use]
pub fn known_models_for(config: &Config, provider_name: &str) -> Vec<String> {
    let mut models = config
        .provider(provider_name)
        .map(|p| p.known_models.clone())
        .unwrap_or_default();
    if let Some(default) = config
        .provider(provider_name)
        .and_then(|p| p.default_model.clone())
        && !models.contains(&default)
    {
        models.insert(0, default);
    }
    models
}

#[must_use]
pub fn all_known_models(config: &Config) -> IndexMap<String, Vec<String>> {
    config
        .providers
        .keys()
        .map(|name| (name.clone(), known_models_for(config, name)))
        .collect()
}

pub fn resolve_fallbacks(config: &Config, primary: &ModelRef) -> Vec<ModelRef> {
    config
        .fallback_models
        .iter()
        .filter_map(|raw| {
            crate::models::resolve_model(config, Some(raw), None)
                .ok()
                .map(|resolved| resolved.reference)
                .filter(|reference| reference != primary)
        })
        .collect()
}

/// Return credentials count for diagnostics without exposing keys.
pub fn configured_credential_providers(paths: &AaykraPaths) -> Vec<String> {
    Credentials::load(paths)
        .map(|creds| creds.providers.keys().cloned().collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_handles_split_chunks_and_crlf() {
        let mut parser = SseBuffer::new();
        let mut events = parser.push(b"event: message_start\r\ndata: {\"a\":");
        assert!(events.is_empty());
        events.extend(parser.push(b"1}\r\n\r\ndata: [DONE]\r\n\r\n"));
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event.as_deref(), Some("message_start"));
        assert_eq!(events[1].data, "[DONE]");
    }

    #[test]
    fn sse_handles_unicode_split_mid_codepoint() {
        let block = "data: héllo\n\n".as_bytes();
        let split = 8; // inside the two-byte é
        let mut parser = SseBuffer::new();
        assert!(parser.push(&block[..split]).is_empty());
        let events = parser.push(&block[split..]);
        assert_eq!(events[0].data, "héllo");
    }

    #[test]
    fn joins_urls_without_double_slashes() {
        assert_eq!(join_url("http://x/v1/", "/chat"), "http://x/v1/chat");
    }

    #[test]
    fn refresh_usage_variants() {
        let mut usage = Usage::default();
        refresh_usage(
            &mut usage,
            &serde_json::json!({"usage":{"prompt_tokens":10,"completion_tokens":5,"completion_tokens_details":{"reasoning_tokens":2}}}),
        );
        assert_eq!(usage.input_tokens, 10);
        assert_eq!(usage.reasoning_tokens, 2);
    }
}
