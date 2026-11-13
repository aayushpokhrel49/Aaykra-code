//! Model Context Protocol client: local stdio and remote streamable HTTP.
//!
//! MCP tools are exposed through a single meta-tool by default. That keeps the
//! model's tool schema small and implements deferred loading; discovery still
//! returns every real tool, schema, and resource.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use indexmap::IndexMap;
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, Command};
use tokio::sync::{Mutex, oneshot};

use crate::config::McpServerConfig;
use crate::providers::SseBuffer;

const MCP_PROTOCOL_VERSION: &str = "2025-06-18";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpTool {
    pub server: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default, rename = "inputSchema")]
    pub input_schema: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpResource {
    pub server: String,
    pub uri: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default, rename = "mimeType")]
    pub mime_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpPrompt {
    pub server: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone, Default)]
pub struct McpDiscovery {
    pub tools: Vec<McpTool>,
    pub resources: Vec<McpResource>,
    pub prompts: Vec<McpPrompt>,
    pub warnings: Vec<String>,
}

pub struct McpManager {
    configs: IndexMap<String, McpServerConfig>,
    cwd: PathBuf,
    clients: Mutex<HashMap<String, Arc<McpClient>>>,
    errors: Mutex<HashMap<String, String>>,
    http: reqwest::Client,
}

impl McpManager {
    pub fn new(
        configs: IndexMap<String, McpServerConfig>,
        cwd: PathBuf,
        http: reqwest::Client,
    ) -> Self {
        Self {
            configs,
            cwd,
            clients: Mutex::new(HashMap::new()),
            errors: Mutex::new(HashMap::new()),
            http,
        }
    }

    #[must_use]
    pub fn configured_names(&self) -> Vec<String> {
        self.configs
            .iter()
            .filter(|(_, config)| config.enabled)
            .map(|(name, _)| name.clone())
            .collect()
    }

    #[must_use]
    pub fn has_servers(&self) -> bool {
        self.configs.values().any(|config| config.enabled)
    }

    async fn client(&self, server: &str) -> Result<Arc<McpClient>> {
        {
            let clients = self.clients.lock().await;
            if let Some(client) = clients.get(server) {
                return Ok(client.clone());
            }
        }
        let config = self
            .configs
            .get(server)
            .with_context(|| format!("unknown MCP server `{server}`"))?;
        if !config.enabled {
            bail!("MCP server `{server}` is disabled");
        }
        let mut clients = self.clients.lock().await;
        if let Some(client) = clients.get(server) {
            return Ok(client.clone());
        }
        let client = match McpClient::spawn(server, config, &self.cwd, self.http.clone()).await {
            Ok(client) => Arc::new(client),
            Err(error) => {
                let message = format!("{error:#}");
                self.errors
                    .lock()
                    .await
                    .insert(server.to_string(), message.clone());
                bail!("{message}");
            }
        };
        clients.insert(server.to_string(), client.clone());
        self.errors.lock().await.remove(server);
        Ok(client)
    }

    pub async fn discover(&self, only: Option<&[String]>) -> McpDiscovery {
        let mut discovery = McpDiscovery::default();
        for (name, config) in &self.configs {
            if !config.enabled {
                continue;
            }
            if let Some(only) = only
                && !only.is_empty()
                && !only.iter().any(|allowed| allowed == name)
            {
                continue;
            }
            let client = match self.client(name).await {
                Ok(client) => client,
                Err(error) => {
                    discovery.warnings.push(format!("{name}: {error:#}"));
                    continue;
                }
            };
            match client.list_tools().await {
                Ok(tools) => discovery.tools.extend(tools.into_iter().map(|mut tool| {
                    tool.server = name.clone();
                    tool
                })),
                Err(error) => discovery
                    .warnings
                    .push(format!("{name}: tools/list failed: {error:#}")),
            }
            match client.list_resources().await {
                Ok(resources) => {
                    discovery
                        .resources
                        .extend(resources.into_iter().map(|mut resource| {
                            resource.server = name.clone();
                            resource
                        }));
                }
                Err(error) => discovery
                    .warnings
                    .push(format!("{name}: resources/list failed: {error:#}")),
            }
            match client.list_prompts().await {
                Ok(prompts) => discovery
                    .prompts
                    .extend(prompts.into_iter().map(|mut prompt| {
                        prompt.server = name.clone();
                        prompt
                    })),
                Err(error) => discovery
                    .warnings
                    .push(format!("{name}: prompts/list failed: {error:#}")),
            }
        }
        discovery
    }

    pub async fn call_tool(&self, server: &str, tool: &str, arguments: Value) -> Result<String> {
        let client = self.client(server).await?;
        let result = client
            .transport
            .request(
                "tools/call",
                json!({"name": tool, "arguments": arguments}),
                client.timeout(),
            )
            .await?;
        Ok(render_tool_result(&result))
    }

    pub async fn read_resource(&self, server: &str, uri: &str) -> Result<String> {
        let client = self.client(server).await?;
        let result = client
            .transport
            .request("resources/read", json!({"uri": uri}), client.timeout())
            .await?;
        Ok(render_resource_result(&result))
    }

    pub async fn get_prompt(&self, server: &str, name: &str, arguments: Value) -> Result<String> {
        let client = self.client(server).await?;
        let result = client
            .transport
            .request(
                "prompts/get",
                json!({"name": name, "arguments": arguments}),
                client.timeout(),
            )
            .await?;
        let mut output = String::new();
        if let Some(description) = result.get("description").and_then(Value::as_str) {
            output.push_str(description);
            output.push_str("\n\n");
        }
        if let Some(messages) = result.get("messages").and_then(Value::as_array) {
            for message in messages {
                let role = message
                    .get("role")
                    .and_then(Value::as_str)
                    .unwrap_or("user");
                let content = message.get("content").cloned().unwrap_or(Value::Null);
                output.push_str(&format!("[{role}]\n"));
                output.push_str(&render_content_blocks(&content));
                output.push_str("\n\n");
            }
        }
        Ok(output.trim().to_string())
    }

    pub async fn render_status(&self) -> String {
        if self.configs.is_empty() {
            return "No MCP servers configured.".into();
        }
        let clients = self.clients.lock().await;
        let errors = self.errors.lock().await;
        let mut lines = vec!["MCP servers:".to_string()];
        for (name, config) in &self.configs {
            let kind = if config.command.is_some() {
                "stdio"
            } else {
                "http"
            };
            let state = if !config.enabled {
                "disabled"
            } else if clients.contains_key(name) {
                "connected"
            } else if errors.contains_key(name) {
                "error"
            } else {
                "deferred"
            };
            lines.push(format!("  {name:<20} {kind:<6} {state}"));
        }
        for (name, error) in errors.iter() {
            lines.push(format!("  ! {name}: {error}"));
        }
        lines.join("\n")
    }

    #[must_use]
    pub fn render_discovery(discovery: &McpDiscovery) -> String {
        let mut out = String::new();
        if !discovery.tools.is_empty() {
            out.push_str(&format!("Tools ({}):\n", discovery.tools.len()));
            for tool in &discovery.tools {
                out.push_str(&format!(
                    "  {}/{}  {}\n",
                    tool.server,
                    tool.name,
                    truncate(&tool.description, 110)
                ));
            }
        }
        if !discovery.resources.is_empty() {
            out.push_str(&format!("\nResources ({}):\n", discovery.resources.len()));
            for resource in &discovery.resources {
                out.push_str(&format!(
                    "  {}/{}  {}\n",
                    resource.server, resource.uri, resource.name
                ));
            }
        }
        if !discovery.prompts.is_empty() {
            out.push_str(&format!("\nPrompts ({}):\n", discovery.prompts.len()));
            for prompt in &discovery.prompts {
                out.push_str(&format!(
                    "  {}/{}  {}\n",
                    prompt.server,
                    prompt.name,
                    truncate(&prompt.description, 100)
                ));
            }
        }
        if !discovery.warnings.is_empty() {
            out.push_str("\nWarnings:\n");
            for warning in &discovery.warnings {
                out.push_str(&format!("  ! {warning}\n"));
            }
        }
        if out.is_empty() {
            out.push_str("No MCP tools, resources, or prompts discovered.");
        }
        out
    }

    pub async fn shutdown(&self) {
        let clients: Vec<Arc<McpClient>> = {
            let mut clients = self.clients.lock().await;
            clients.drain().map(|(_, client)| client).collect()
        };
        for client in clients {
            client.transport.close().await;
        }
    }
}

struct McpClient {
    name: String,
    transport: Arc<dyn McpTransport>,
    timeout: Duration,
}

impl McpClient {
    async fn spawn(
        name: &str,
        config: &McpServerConfig,
        cwd: &std::path::Path,
        http: reqwest::Client,
    ) -> Result<Self> {
        let timeout = Duration::from_millis(config.timeout_ms.clamp(1_000, 300_000));
        let transport: Arc<dyn McpTransport> = if let Some(command) = &config.command {
            Arc::new(
                StdioTransport::spawn(command, &config.args, &config.env, cwd)
                    .context("failed to start stdio MCP server")?,
            )
        } else if let Some(url) = &config.url {
            Arc::new(HttpTransport::new(
                http,
                url.clone(),
                &config.headers,
                timeout,
            )?)
        } else {
            bail!("MCP server `{name}` has neither command nor url");
        };

        let init = transport
            .request(
                "initialize",
                json!({
                    "protocolVersion": MCP_PROTOCOL_VERSION,
                    "capabilities": {
                        "roots": {"listChanged": false},
                        "sampling": {}
                    },
                    "clientInfo": {
                        "name": "aaykra-code",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                }),
                timeout,
            )
            .await
            .with_context(|| format!("MCP initialization failed for `{name}`"))?;
        let _ = init;
        transport
            .notify("notifications/initialized", json!({}))
            .await
            .ok();
        Ok(Self {
            name: name.to_string(),
            transport,
            timeout,
        })
    }

    fn timeout(&self) -> Duration {
        self.timeout
    }

    async fn list_tools(&self) -> Result<Vec<McpTool>> {
        let result = self
            .transport
            .request("tools/list", json!({}), self.timeout)
            .await?;
        Ok(result
            .get("tools")
            .and_then(Value::as_array)
            .map(|tools| {
                tools
                    .iter()
                    .filter_map(|tool| {
                        let name = tool.get("name").and_then(Value::as_str)?;
                        Some(McpTool {
                            server: self.name.clone(),
                            name: name.to_string(),
                            description: tool
                                .get("description")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string(),
                            input_schema: tool
                                .get("inputSchema")
                                .cloned()
                                .unwrap_or_else(|| json!({"type": "object"})),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn list_resources(&self) -> Result<Vec<McpResource>> {
        let result = self
            .transport
            .request("resources/list", json!({}), self.timeout)
            .await?;
        Ok(result
            .get("resources")
            .and_then(Value::as_array)
            .map(|resources| {
                resources
                    .iter()
                    .filter_map(|resource| {
                        let uri = resource.get("uri").and_then(Value::as_str)?;
                        Some(McpResource {
                            server: self.name.clone(),
                            uri: uri.to_string(),
                            name: resource
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or(uri)
                                .to_string(),
                            description: resource
                                .get("description")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string(),
                            mime_type: resource
                                .get("mimeType")
                                .and_then(Value::as_str)
                                .map(ToOwned::to_owned),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn list_prompts(&self) -> Result<Vec<McpPrompt>> {
        let result = self
            .transport
            .request("prompts/list", json!({}), self.timeout)
            .await?;
        Ok(result
            .get("prompts")
            .and_then(Value::as_array)
            .map(|prompts| {
                prompts
                    .iter()
                    .filter_map(|prompt| {
                        let name = prompt.get("name").and_then(Value::as_str)?;
                        Some(McpPrompt {
                            server: self.name.clone(),
                            name: name.to_string(),
                            description: prompt
                                .get("description")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default())
    }
}

#[async_trait]
trait McpTransport: Send + Sync {
    async fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value>;
    async fn notify(&self, method: &str, params: Value) -> Result<()>;
    async fn close(&self);
}

struct StdioTransport {
    stdin: Arc<Mutex<ChildStdin>>,
    pending: Arc<Mutex<HashMap<i64, oneshot::Sender<Result<Value, String>>>>>,
    next_id: AtomicI64,
    _child: Mutex<tokio::process::Child>,
    reader: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl StdioTransport {
    fn spawn(
        command: &str,
        args: &[String],
        env: &IndexMap<String, String>,
        cwd: &std::path::Path,
    ) -> Result<Self> {
        let mut cmd = Command::new(command);
        cmd.args(args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        for (key, value) in env {
            cmd.env(key, value);
        }
        let mut child = cmd
            .spawn()
            .with_context(|| format!("could not spawn `{command}`"))?;
        let stdin = child.stdin.take().context("MCP stdin unavailable")?;
        let stdout = child.stdout.take().context("MCP stdout unavailable")?;
        let stdin = Arc::new(Mutex::new(stdin));
        let pending: Arc<Mutex<HashMap<i64, oneshot::Sender<Result<Value, String>>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let reader = tokio::spawn({
            let stdin = stdin.clone();
            let pending = pending.clone();
            let cwd = cwd.to_path_buf();
            async move {
                stdio_reader(stdout, stdin, pending, cwd).await;
            }
        });
        Ok(Self {
            stdin,
            pending,
            next_id: AtomicI64::new(1),
            _child: Mutex::new(child),
            reader: Mutex::new(Some(reader)),
        })
    }
}

#[async_trait]
impl McpTransport for StdioTransport {
    async fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        self.pending.lock().await.insert(id, sender);
        if let Err(error) = write_json_line(
            &self.stdin,
            &json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}),
        )
        .await
        {
            self.pending.lock().await.remove(&id);
            return Err(error);
        }
        match tokio::time::timeout(timeout, receiver).await {
            Ok(Ok(Ok(value))) => Ok(value),
            Ok(Ok(Err(message))) => bail!("{message}"),
            Ok(Err(_)) => bail!("MCP server closed the connection"),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                bail!("MCP request `{method}` timed out")
            }
        }
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        write_json_line(
            &self.stdin,
            &json!({"jsonrpc": "2.0", "method": method, "params": params}),
        )
        .await
    }

    async fn close(&self) {
        let _ = self.notify("notifications/cancelled", json!({})).await;
        if let Some(handle) = self.reader.lock().await.take() {
            handle.abort();
        }
        if let Ok(mut child) = self._child.try_lock() {
            child.start_kill().ok();
        }
    }
}

async fn write_json_line(stdin: &Arc<Mutex<ChildStdin>>, value: &Value) -> Result<()> {
    let mut stdin = stdin.lock().await;
    stdin
        .write_all(serde_json::to_string(value)?.as_bytes())
        .await?;
    stdin.write_all(b"\n").await?;
    stdin.flush().await?;
    Ok(())
}

async fn stdio_reader(
    stdout: tokio::process::ChildStdout,
    stdin: Arc<Mutex<ChildStdin>>,
    pending: Arc<Mutex<HashMap<i64, oneshot::Sender<Result<Value, String>>>>>,
    cwd: PathBuf,
) {
    let mut lines = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if message.get("method").is_some() {
            if let Some(id) = message.get("id").cloned() {
                let method = message.get("method").and_then(Value::as_str).unwrap_or("");
                let response = if method == "roots/list" {
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {"roots": [{"uri": url::Url::from_directory_path(&cwd).map(|u| u.to_string()).unwrap_or_else(|_| "file:///".into()), "name": "workspace"}]}
                    })
                } else {
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {"code": -32601, "message": format!("unsupported MCP client request: {method}")}
                    })
                };
                if write_json_line(&stdin, &response).await.is_err() {
                    break;
                }
            }
            continue;
        }
        let Some(id) = message.get("id").and_then(Value::as_i64) else {
            continue;
        };
        if let Some(sender) = pending.lock().await.remove(&id) {
            if let Some(error) = message.get("error") {
                let text = error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown MCP error")
                    .to_string();
                let _ = sender.send(Err(text));
            } else {
                let _ = sender.send(Ok(message.get("result").cloned().unwrap_or(Value::Null)));
            }
        }
    }
    let mut pending = pending.lock().await;
    for (_, sender) in pending.drain() {
        let _ = sender.send(Err("MCP server closed".into()));
    }
}

struct HttpTransport {
    client: reqwest::Client,
    url: String,
    headers: HeaderMap,
    session_id: Mutex<Option<String>>,
    next_id: AtomicI64,
    timeout: Duration,
}

impl HttpTransport {
    fn new(
        client: reqwest::Client,
        url: String,
        custom_headers: &IndexMap<String, String>,
        timeout: Duration,
    ) -> Result<Self> {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("application/json, text/event-stream"),
        );
        headers.insert(
            HeaderName::from_static("mcp-protocol-version"),
            HeaderValue::from_static(MCP_PROTOCOL_VERSION),
        );
        for (name, value) in custom_headers {
            headers.insert(
                HeaderName::from_bytes(name.as_bytes()).context("invalid MCP header name")?,
                HeaderValue::from_str(value).context("invalid MCP header value")?,
            );
        }
        Ok(Self {
            client,
            url,
            headers,
            session_id: Mutex::new(None),
            next_id: AtomicI64::new(1),
            timeout,
        })
    }
}

#[async_trait]
impl McpTransport for HttpTransport {
    async fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        });
        let mut builder = self
            .client
            .post(&self.url)
            .headers(self.headers.clone())
            .timeout(timeout.min(self.timeout))
            .json(&request);
        if let Some(session_id) = self.session_id.lock().await.as_deref() {
            builder = builder.header("Mcp-Session-Id", session_id);
        }
        let response = builder.send().await.context("MCP HTTP request failed")?;
        if let Some(session_id) = response
            .headers()
            .get("mcp-session-id")
            .and_then(|value| value.to_str().ok())
        {
            *self.session_id.lock().await = Some(session_id.to_string());
        }
        let status = response.status();
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("MCP HTTP {}: {}", status, truncate(&body, 2_000));
        }
        if body.trim().is_empty() {
            return Ok(Value::Null);
        }
        let values: Vec<Value> = if content_type.contains("text/event-stream") {
            let mut parser = SseBuffer::new();
            let mut events = parser.push(body.as_bytes());
            events.extend(parser.flush());
            events
                .into_iter()
                .filter_map(|event| serde_json::from_str(&event.data).ok())
                .collect()
        } else {
            vec![serde_json::from_str(&body).with_context(|| {
                format!("invalid MCP HTTP response: {}", truncate(&body, 1_000))
            })?]
        };
        for value in values {
            if value.get("id").and_then(Value::as_i64) == Some(id) {
                if let Some(error) = value.get("error") {
                    bail!(
                        "MCP error: {}",
                        error
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown error")
                    );
                }
                return Ok(value.get("result").cloned().unwrap_or(Value::Null));
            }
        }
        bail!("MCP HTTP response did not include a matching response")
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        let mut builder = self
            .client
            .post(&self.url)
            .headers(self.headers.clone())
            .timeout(self.timeout)
            .json(&json!({
                "jsonrpc": "2.0",
                "method": method,
                "params": params
            }));
        if let Some(session_id) = self.session_id.lock().await.as_deref() {
            builder = builder.header("Mcp-Session-Id", session_id);
        }
        let response = builder.send().await.context("MCP notification failed")?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            bail!(
                "MCP notification returned {status}: {}",
                truncate(&body, 500)
            );
        }
        Ok(())
    }

    async fn close(&self) {
        let mut request = self
            .client
            .delete(&self.url)
            .headers(self.headers.clone())
            .timeout(self.timeout);
        if let Some(session_id) = self.session_id.lock().await.as_deref() {
            request = request.header("Mcp-Session-Id", session_id);
        }
        let _ = request.send().await;
    }
}

fn render_tool_result(result: &Value) -> String {
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        let content = result.get("content").cloned().unwrap_or(Value::Null);
        return format!(
            "MCP tool returned an error:\n{}",
            render_content_blocks(&content)
        );
    }
    let content = result.get("content").cloned().unwrap_or(Value::Null);
    let text = render_content_blocks(&content);
    if !text.trim().is_empty() {
        if let Some(structured) = result.get("structuredContent") {
            return format!(
                "{}\n\nStructured content:\n{}",
                text,
                serde_json::to_string_pretty(structured).unwrap_or_default()
            );
        }
        return text;
    }
    if let Some(structured) = result.get("structuredContent") {
        return serde_json::to_string_pretty(structured).unwrap_or_default();
    }
    serde_json::to_string_pretty(result).unwrap_or_default()
}

fn render_content_blocks(content: &Value) -> String {
    if let Some(text) = content.as_str() {
        return text.to_string();
    }
    let Some(items) = content.as_array() else {
        return String::new();
    };
    let mut out = Vec::new();
    for item in items {
        match item.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = item.get("text").and_then(Value::as_str) {
                    out.push(text.to_string());
                }
            }
            Some("image") => out.push(format!(
                "[image: {}]",
                item.get("mimeType")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
            )),
            Some("audio") => out.push("[audio content]".to_string()),
            Some("resource") => {
                if let Some(resource) = item.get("resource") {
                    if let Some(text) = resource.get("text").and_then(Value::as_str) {
                        out.push(text.to_string());
                    } else {
                        out.push(format!(
                            "[resource: {}]",
                            resource.get("uri").and_then(Value::as_str).unwrap_or("")
                        ));
                    }
                }
            }
            Some("resource_link") => out.push(format!(
                "[resource link: {}]",
                item.get("uri").and_then(Value::as_str).unwrap_or("")
            )),
            _ => {}
        }
    }
    out.join("\n\n")
}

fn render_resource_result(result: &Value) -> String {
    let Some(contents) = result.get("contents").and_then(Value::as_array) else {
        return serde_json::to_string_pretty(result).unwrap_or_default();
    };
    let mut out = Vec::new();
    for item in contents {
        let uri = item.get("uri").and_then(Value::as_str).unwrap_or("");
        if let Some(text) = item.get("text").and_then(Value::as_str) {
            out.push(format!("{uri}:\n{text}"));
        } else if let Some(blob) = item.get("blob").and_then(Value::as_str) {
            out.push(format!(
                "{uri}: [binary resource, {} base64 chars, type {}]",
                blob.len(),
                item.get("mimeType")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
            ));
        }
    }
    out.join("\n\n")
}

fn truncate(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        value.to_string()
    } else {
        format!(
            "{}…",
            value
                .chars()
                .take(max.saturating_sub(1))
                .collect::<String>()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_text_tool_result() {
        let result = json!({"content": [{"type": "text", "text": "hello"}]});
        assert_eq!(render_tool_result(&result), "hello");
    }

    #[test]
    fn renders_error_tool_result() {
        let result = json!({
            "isError": true,
            "content": [{"type": "text", "text": "bad"}]
        });
        assert!(render_tool_result(&result).contains("error"));
    }

    #[test]
    fn renders_resource_text() {
        let result = json!({"contents":[{"uri":"file://x","text":"content"}]});
        assert!(render_resource_result(&result).contains("content"));
    }
}
