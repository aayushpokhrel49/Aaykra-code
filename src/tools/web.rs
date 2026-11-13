//! Web search (DuckDuckGo, Brave, Tavily) and fetch-to-text tools.

use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use futures_util::StreamExt;
use regex::Regex;
use serde_json::{Value, json};
use url::Url;

use crate::tools::{Tool, ToolContext, definition, permission_request, truncate_tool_output};
use crate::types::{RiskLevel, ToolDefinition};

const SEARCH_TIMEOUT: Duration = Duration::from_secs(25);
const FETCH_TIMEOUT: Duration = Duration::from_secs(45);

pub struct WebSearchTool;
pub struct WebFetchTool;

#[derive(Debug, Clone)]
struct SearchResult {
    title: String,
    url: String,
    snippet: String,
}

#[async_trait]
impl Tool for WebSearchTool {
    fn definition(&self) -> ToolDefinition {
        definition(
            "WebSearch",
            "Search the public web and return ranked titles, URLs, and snippets. Uses DuckDuckGo HTML by default, or Brave/Tavily when configured with an API key. Follow up with WebFetch to read a promising result.",
            json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Search query."},
                    "max_results": {"type": "integer", "minimum": 1, "maximum": 20, "description": "Result count. Defaults to the configured value (8)."}
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        )
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn execute(&self, input: Value, context: &ToolContext) -> Result<String> {
        context.ensure_not_cancelled()?;
        let query = input
            .get("query")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .context("missing `query`")?;
        let configured = context.runtime.config.web.max_results.clamp(1, 20);
        let limit = input
            .get("max_results")
            .and_then(Value::as_u64)
            .map_or(configured, |value| value.clamp(1, 20) as usize);
        let mut request = permission_request(
            "WebSearch",
            query.to_string(),
            "search the web",
            true,
            RiskLevel::Low,
            Vec::new(),
        );
        request.suggested_rule = "WebSearch".into();
        context.authorize(request).await?;

        let http = context.runtime.provider_runtime.http();
        let web = &context.runtime.config.web;
        let provider = web.search_provider.trim().to_ascii_lowercase();
        let provider = if provider == "auto" || provider.is_empty() {
            if web.brave_api_key.is_some() {
                "brave".to_string()
            } else if web.tavily_api_key.is_some() {
                "tavily".to_string()
            } else {
                "duckduckgo".to_string()
            }
        } else {
            provider
        };
        let (used, results) = match provider.as_str() {
            "duckduckgo" | "ddg" => (
                "duckduckgo",
                duckduckgo(http, query, limit)
                    .await
                    .context("DuckDuckGo search failed")?,
            ),
            "brave" => {
                let key = web
                    .brave_api_key
                    .as_deref()
                    .context("web.brave_api_key is not configured")?;
                (
                    "brave",
                    brave(http, query, limit, key)
                        .await
                        .context("Brave search failed")?,
                )
            }
            "tavily" => {
                let key = web
                    .tavily_api_key
                    .as_deref()
                    .context("web.tavily_api_key is not configured")?;
                (
                    "tavily",
                    tavily(http, query, limit, key)
                        .await
                        .context("Tavily search failed")?,
                )
            }
            other => {
                bail!("unknown web.search_provider `{other}` (duckduckgo, brave, tavily, auto)")
            }
        };
        if results.is_empty() {
            return Ok(format!(
                "No results for \"{query}\" ({used}). Try a different query."
            ));
        }
        let mut out = format!("Search results for \"{query}\" via {used}:\n");
        for (index, result) in results.iter().enumerate() {
            out.push_str(&format!(
                "\n{}. {}\n   {}\n",
                index + 1,
                result.title.trim(),
                result.url.trim()
            ));
            let snippet = result.snippet.trim();
            if !snippet.is_empty() {
                let snippet: String = snippet.chars().take(400).collect();
                out.push_str(&format!("   {snippet}\n"));
            }
        }
        Ok(truncate_tool_output(context, out))
    }
}

async fn duckduckgo(
    http: &reqwest::Client,
    query: &str,
    limit: usize,
) -> Result<Vec<SearchResult>> {
    let mut url = Url::parse("https://html.duckduckgo.com/html/")?;
    url.query_pairs_mut().append_pair("q", query);
    let response = http
        .get(url)
        .timeout(SEARCH_TIMEOUT)
        .header(reqwest::header::ACCEPT, "text/html,application/xhtml+xml")
        .send()
        .await
        .context("could not reach DuckDuckGo")?;
    let status = response.status();
    let body = response
        .text()
        .await
        .context("could not read DuckDuckGo response")?;
    if !status.is_success() {
        bail!("DuckDuckGo returned HTTP {status}");
    }
    if body.contains("anomaly-modal") || body.contains("bots use DuckDuckGo") {
        bail!(
            "DuckDuckGo rejected the request as automated; configure web.brave_api_key or web.tavily_api_key for reliable search"
        );
    }

    static ANCHOR: OnceLock<Regex> = OnceLock::new();
    static HREF: OnceLock<Regex> = OnceLock::new();
    static SNIPPET: OnceLock<Regex> = OnceLock::new();
    let anchor = ANCHOR.get_or_init(|| {
        Regex::new(r#"(?is)<a\b([^>]*class="[^"]*result__a[^"]*"[^>]*)>(.*?)</a>"#).unwrap()
    });
    let href = HREF.get_or_init(|| Regex::new(r#"(?is)href="([^"]+)""#).unwrap());
    let snippet = SNIPPET.get_or_init(|| {
        Regex::new(
            r#"(?is)<(?:a|div)\b[^>]*class="[^"]*result__snippet[^"]*"[^>]*>(.*?)</(?:a|div)>"#,
        )
        .unwrap()
    });
    let snippets: Vec<String> = snippet
        .captures_iter(&body)
        .map(|captures| clean_html(&captures[1]))
        .collect();
    let mut results = Vec::new();
    for (index, captures) in anchor.captures_iter(&body).enumerate() {
        if results.len() >= limit {
            break;
        }
        let attributes = &captures[1];
        let Some(link) = href
            .captures(attributes)
            .map(|captures| captures[1].to_string())
        else {
            continue;
        };
        let title = clean_html(&captures[2]);
        let url = decode_duckduckgo_link(&link);
        if title.is_empty() || url.is_empty() {
            continue;
        }
        results.push(SearchResult {
            title,
            url,
            snippet: snippets.get(index).cloned().unwrap_or_default(),
        });
    }
    Ok(results)
}

async fn brave(
    http: &reqwest::Client,
    query: &str,
    limit: usize,
    key: &str,
) -> Result<Vec<SearchResult>> {
    let response = http
        .get("https://api.search.brave.com/res/v1/web/search")
        .query(&[("q", query), ("count", &limit.to_string())])
        .timeout(SEARCH_TIMEOUT)
        .header("X-Subscription-Token", key)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .context("could not reach Brave Search")?;
    let status = response.status();
    let body: Value = response
        .json()
        .await
        .context("Brave returned a non-JSON response")?;
    if !status.is_success() {
        let message = body
            .get("error")
            .and_then(|error| error.get("detail"))
            .and_then(Value::as_str)
            .unwrap_or("request rejected");
        bail!("Brave returned HTTP {status}: {message}");
    }
    Ok(body
        .get("web")
        .and_then(|web| web.get("results"))
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .take(limit)
                .filter_map(|item| {
                    Some(SearchResult {
                        title: item.get("title")?.as_str()?.to_string(),
                        url: item.get("url")?.as_str()?.to_string(),
                        snippet: item
                            .get("description")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default())
}

async fn tavily(
    http: &reqwest::Client,
    query: &str,
    limit: usize,
    key: &str,
) -> Result<Vec<SearchResult>> {
    let response = http
        .post("https://api.tavily.com/search")
        .timeout(SEARCH_TIMEOUT)
        .json(&json!({
            "api_key": key,
            "query": query,
            "max_results": limit,
            "search_depth": "basic",
            "include_answer": false
        }))
        .send()
        .await
        .context("could not reach Tavily")?;
    let status = response.status();
    let body: Value = response
        .json()
        .await
        .context("Tavily returned a non-JSON response")?;
    if !status.is_success() {
        let message = body
            .get("detail")
            .and_then(|detail| detail.get("error").or(Some(detail)))
            .and_then(Value::as_str)
            .unwrap_or("request rejected");
        bail!("Tavily returned HTTP {status}: {message}");
    }
    Ok(body
        .get("results")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .take(limit)
                .filter_map(|item| {
                    Some(SearchResult {
                        title: item.get("title")?.as_str()?.to_string(),
                        url: item.get("url")?.as_str()?.to_string(),
                        snippet: item
                            .get("content")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default())
}

fn decode_duckduckgo_link(link: &str) -> String {
    let absolute = if link.starts_with("//") {
        format!("https:{link}")
    } else {
        link.to_string()
    };
    if let Ok(url) = Url::parse(&absolute) {
        for (key, value) in url.query_pairs() {
            if key == "uddg" {
                return value.into_owned();
            }
        }
    }
    absolute
}

fn clean_html(input: &str) -> String {
    static TAGS: OnceLock<Regex> = OnceLock::new();
    static SPACE: OnceLock<Regex> = OnceLock::new();
    let tags = TAGS.get_or_init(|| Regex::new(r"(?is)<[^>]+>").unwrap());
    let space = SPACE.get_or_init(|| Regex::new(r"\s+").unwrap());
    let spaced = tags.replace_all(input, " ");
    decode_entities(space.replace_all(spaced.trim(), " ").as_ref())
}

fn decode_entities(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(position) = rest.find('&') {
        out.push_str(&rest[..position]);
        rest = &rest[position..];
        let Some(end) = rest.find(';') else {
            out.push_str(rest);
            return out;
        };
        let entity = &rest[1..end];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" | "#39" => Some('\''),
            "nbsp" => Some(' '),
            _ => entity
                .strip_prefix('#')
                .and_then(|value| {
                    if let Some(hex) = value.strip_prefix(['x', 'X']) {
                        u32::from_str_radix(hex, 16).ok()
                    } else {
                        value.parse::<u32>().ok()
                    }
                })
                .and_then(char::from_u32),
        };
        if let Some(character) = decoded {
            out.push(character);
            rest = &rest[end + 1..];
        } else {
            out.push('&');
            rest = &rest[1..];
        }
    }
    out.push_str(rest);
    out
}

#[async_trait]
impl Tool for WebFetchTool {
    fn definition(&self) -> ToolDefinition {
        definition(
            "WebFetch",
            "Fetch an http(s) URL and return it as readable text (HTML becomes Markdown-style text, JSON is pretty-printed). Response size is bounded by config. Use this to read documentation, issues, and search results.",
            json!({
                "type": "object",
                "properties": {
                    "url": {"type": "string", "description": "Absolute http or https URL."}
                },
                "required": ["url"],
                "additionalProperties": false
            }),
        )
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn execute(&self, input: Value, context: &ToolContext) -> Result<String> {
        context.ensure_not_cancelled()?;
        let raw = input
            .get("url")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .context("missing `url`")?;
        let url = Url::parse(raw).with_context(|| format!("invalid URL `{raw}`"))?;
        if !matches!(url.scheme(), "http" | "https") {
            bail!("WebFetch only supports http and https URLs");
        }
        if !url.username().is_empty() || url.password().is_some() {
            bail!("URLs with embedded credentials are not allowed");
        }
        let mut request = permission_request(
            "WebFetch",
            url.to_string(),
            "fetch a web page",
            true,
            RiskLevel::Low,
            Vec::new(),
        );
        request.suggested_rule = "WebFetch".into();
        context.authorize(request).await?;

        let max_bytes = context
            .runtime
            .config
            .web
            .fetch_max_bytes
            .clamp(1_024, 20 * 1024 * 1024);
        let response = context
            .runtime
            .provider_runtime
            .http()
            .get(url.clone())
            .timeout(FETCH_TIMEOUT)
            .header(
                reqwest::header::ACCEPT,
                "text/html,application/xhtml+xml,application/json,text/plain;q=0.9,*/*;q=0.5",
            )
            .send()
            .await
            .with_context(|| format!("could not fetch {url}"))?;
        let status = response.status();
        if !status.is_success() {
            bail!("{url} returned HTTP {status}");
        }
        let final_url = response.url().to_string();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("application/octet-stream")
            .to_string();
        let mut bytes = Vec::new();
        let mut truncated = false;
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            context.ensure_not_cancelled()?;
            let chunk = chunk.context("error reading response body")?;
            if bytes.len() + chunk.len() > max_bytes {
                let remaining = max_bytes.saturating_sub(bytes.len());
                bytes.extend_from_slice(&chunk[..remaining.min(chunk.len())]);
                truncated = true;
                break;
            }
            bytes.extend_from_slice(&chunk);
        }
        if bytes.iter().take(8_000).any(|byte| *byte == 0) {
            bail!(
                "{final_url} returned binary data ({}), not text",
                content_type
            );
        }
        let is_html = content_type.contains("html")
            || bytes
                .iter()
                .take(512)
                .copied()
                .collect::<Vec<u8>>()
                .windows(5)
                .any(|window| window.eq_ignore_ascii_case(b"<html"));
        let text = if content_type.contains("json") {
            serde_json::from_slice::<Value>(&bytes)
                .ok()
                .and_then(|value| serde_json::to_string_pretty(&value).ok())
                .unwrap_or_else(|| String::from_utf8_lossy(&bytes).into_owned())
        } else if is_html {
            html2text::from_read(bytes.as_slice(), 100)
                .unwrap_or_else(|_| String::from_utf8_lossy(&bytes).into_owned())
        } else {
            String::from_utf8_lossy(&bytes).into_owned()
        };
        let mut out =
            format!("Fetched {final_url}\ncontent-type: {content_type}\nstatus: {status}\n\n",);
        out.push_str(text.trim());
        if truncated {
            out.push_str(&format!(
                "\n\n… response truncated at {max_bytes} bytes; fetch a narrower page or use an API"
            ));
        }
        Ok(truncate_tool_output(context, out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_duckduckgo_redirect() {
        let link = "//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fa%3Fb%3D1&rut=x";
        assert_eq!(decode_duckduckgo_link(link), "https://example.com/a?b=1");
    }

    #[test]
    fn strips_tags_and_entities() {
        assert_eq!(clean_html("<b>Hello</b> &amp; goodbye"), "Hello & goodbye");
    }

    #[test]
    fn decodes_numeric_entities() {
        assert_eq!(decode_entities("&#65;&#x42;"), "AB");
    }
}
