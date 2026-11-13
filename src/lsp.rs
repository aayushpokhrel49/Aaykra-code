//! A small, dependency-free LSP client for live diagnostics and code
//! intelligence. Aaykra editor can proxy its already-running language servers
//! later through the same tool surface; standalone Aaykra Code starts servers
//! lazily from `[lsp.*]` config or common installed servers.

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{Mutex, oneshot};
use tokio::task::JoinHandle;

use crate::config::LspServerConfig;
use crate::repo_map::relative_display;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LspDiagnostic {
    #[serde(default)]
    pub range: Range,
    #[serde(default)]
    pub severity: Option<u32>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub code: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Location {
    uri: String,
    range: Range,
}

#[derive(Debug, Clone)]
struct DocumentState {
    version: i32,
    hash: u64,
}

pub struct LspManager {
    configs: IndexMap<String, LspServerConfig>,
    root: PathBuf,
    clients: Mutex<HashMap<String, Arc<LspClient>>>,
    auto_detect: bool,
}

impl LspManager {
    #[must_use]
    pub fn new(configs: IndexMap<String, LspServerConfig>, root: PathBuf) -> Self {
        Self {
            configs,
            root,
            clients: Mutex::new(HashMap::new()),
            auto_detect: true,
        }
    }

    #[must_use]
    pub fn has_server_for(&self, path: &Path) -> bool {
        let extension = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        self.matching_config(extension).is_some()
            || (self.auto_detect && auto_config(extension).is_some())
    }

    fn matching_config(&self, extension: &str) -> Option<(String, LspServerConfig)> {
        self.configs.iter().find_map(|(name, config)| {
            if !config.enabled {
                return None;
            }
            let matches = config.extensions.iter().any(|ext| {
                let ext = ext.trim().trim_start_matches('.');
                ext.eq_ignore_ascii_case(extension)
            });
            matches.then(|| (name.clone(), config.clone()))
        })
    }

    fn server_for(&self, path: &Path) -> Option<(String, LspServerConfig)> {
        let extension = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if let Some((name, config)) = self.matching_config(extension) {
            return Some((name, config));
        }
        if self.auto_detect
            && let Some(config) = auto_config(extension)
        {
            return Some((format!("auto-{extension}"), config));
        }
        None
    }

    async fn client_for(&self, path: &Path) -> Result<Arc<LspClient>> {
        let Some((name, config)) = self.server_for(path) else {
            bail!(
                "no LSP server configured for .{} files; add [lsp.<name>] to config",
                path.extension().and_then(|e| e.to_str()).unwrap_or("?")
            );
        };
        let mut clients = self.clients.lock().await;
        if let Some(client) = clients.get(&name) {
            return Ok(client.clone());
        }
        let client = Arc::new(
            LspClient::spawn(&name, &config, &self.root)
                .await
                .with_context(|| format!("could not start LSP server `{name}`"))?,
        );
        clients.insert(name, client.clone());
        Ok(client)
    }

    pub async fn diagnostics(&self, path: &Path, wait: Duration) -> Result<Vec<LspDiagnostic>> {
        let client = self.client_for(path).await?;
        client.open_or_update(path).await?;
        client.wait_for_diagnostics(path, wait).await
    }

    pub async fn definition(&self, path: &Path, line: u32, column: u32) -> Result<String> {
        let client = self.client_for(path).await?;
        client.open_or_update(path).await?;
        let (uri, position) = client.position(path, line, column).await?;
        let result = client
            .request(
                "textDocument/definition",
                json!({"textDocument": {"uri": uri}, "position": position}),
                Duration::from_secs(20),
            )
            .await?;
        Ok(render_locations(&result, &self.root))
    }

    pub async fn references(&self, path: &Path, line: u32, column: u32) -> Result<String> {
        let client = self.client_for(path).await?;
        client.open_or_update(path).await?;
        let (uri, position) = client.position(path, line, column).await?;
        let result = client
            .request(
                "textDocument/references",
                json!({
                    "textDocument": {"uri": uri},
                    "position": position,
                    "context": {"includeDeclaration": true}
                }),
                Duration::from_secs(20),
            )
            .await?;
        Ok(render_locations(&result, &self.root))
    }

    pub async fn hover(&self, path: &Path, line: u32, column: u32) -> Result<String> {
        let client = self.client_for(path).await?;
        client.open_or_update(path).await?;
        let (uri, position) = client.position(path, line, column).await?;
        let result = client
            .request(
                "textDocument/hover",
                json!({"textDocument": {"uri": uri}, "position": position}),
                Duration::from_secs(20),
            )
            .await?;
        Ok(render_hover(&result))
    }

    #[must_use]
    pub fn render_status(&self) -> String {
        let mut lines = vec!["LSP servers:".to_string()];
        if !self.configs.is_empty() {
            for (name, config) in &self.configs {
                let state = if config.enabled {
                    "enabled"
                } else {
                    "disabled"
                };
                lines.push(format!(
                    "  {name:<20} {state:<8} {} {}",
                    config.command,
                    config.args.join(" ")
                ));
            }
        }
        if self.auto_detect {
            lines.push("  Auto-detection enabled (rust-analyzer, pyright/pylsp, typescript-language-server, gopls, clangd, bash-language-server).".into());
        }
        lines.join("\n")
    }

    pub async fn shutdown(&self) {
        let clients: Vec<Arc<LspClient>> = {
            let mut map = self.clients.lock().await;
            map.drain().map(|(_, client)| client).collect()
        };
        for client in clients {
            client.shutdown().await;
        }
    }
}

struct LspClient {
    _name: String,
    _child: Mutex<Child>,
    stdin: Arc<Mutex<ChildStdin>>,
    pending: Arc<Mutex<HashMap<i64, oneshot::Sender<Result<Value, String>>>>>,
    next_id: AtomicI64,
    diagnostics: Arc<Mutex<HashMap<String, Vec<LspDiagnostic>>>>,
    documents: Mutex<HashMap<String, DocumentState>>,
    reader: Mutex<Option<JoinHandle<()>>>,
    root: PathBuf,
}

impl LspClient {
    async fn spawn(name: &str, config: &LspServerConfig, root: &Path) -> Result<Self> {
        let mut command = Command::new(&config.command);
        command
            .args(&config.args)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        for (key, value) in &config.env {
            command.env(key, value);
        }
        let mut child = command
            .spawn()
            .with_context(|| format!("failed to spawn `{}`", config.command))?;
        let stdin = child.stdin.take().context("LSP stdin unavailable")?;
        let stdout = child.stdout.take().context("LSP stdout unavailable")?;
        let stdin = Arc::new(Mutex::new(stdin));
        let pending: Arc<Mutex<HashMap<i64, oneshot::Sender<Result<Value, String>>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let diagnostics = Arc::new(Mutex::new(HashMap::new()));
        let root = root.to_path_buf();

        let reader = tokio::spawn({
            let pending = pending.clone();
            let stdin = stdin.clone();
            let diagnostics = diagnostics.clone();
            let root = root.clone();
            async move {
                reader_loop(stdout, stdin, pending, diagnostics, root).await;
            }
        });

        let client = Self {
            _name: name.to_string(),
            _child: Mutex::new(child),
            stdin,
            pending,
            next_id: AtomicI64::new(1),
            diagnostics,
            documents: Mutex::new(HashMap::new()),
            reader: Mutex::new(Some(reader)),
            root,
        };
        client.initialize(config).await?;
        Ok(client)
    }

    async fn initialize(&self, config: &LspServerConfig) -> Result<()> {
        let root_uri = url::Url::from_directory_path(&self.root)
            .map(|url| url.to_string())
            .unwrap_or_else(|_| "file:///".into());
        let params = json!({
            "processId": std::process::id(),
            "clientInfo": {
                "name": "aaykra-code",
                "version": env!("CARGO_PKG_VERSION")
            },
            "rootPath": self.root,
            "rootUri": root_uri,
            "initializationOptions": if config.initialization_options.is_null() {
                Value::Null
            } else {
                config.initialization_options.clone()
            },
            "capabilities": {
                "workspace": {
                    "workspaceFolders": true,
                    "configuration": true,
                    "didChangeConfiguration": {"dynamicRegistration": false}
                },
                "textDocument": {
                    "synchronization": {
                        "dynamicRegistration": false,
                        "willSave": false,
                        "willSaveWaitUntil": false,
                        "didSave": false
                    },
                    "publishDiagnostics": {"relatedInformation": true},
                    "hover": {"contentFormat": ["markdown", "plaintext"]},
                    "definition": {"linkSupport": true},
                    "references": {},
                    "diagnostic": {"dynamicRegistration": false}
                },
                "general": {"positionEncodings": ["utf-16"]},
                "window": {"workDoneProgress": true}
            },
            "workspaceFolders": [{
                "uri": root_uri,
                "name": self.root.file_name().and_then(|n| n.to_str()).unwrap_or("workspace")
            }],
            "trace": "off"
        });
        let result = self
            .request("initialize", params, Duration::from_secs(60))
            .await
            .context("LSP initialize failed")?;
        let _ = result;
        self.notify("initialized", json!({})).await?;
        Ok(())
    }

    async fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        self.pending.lock().await.insert(id, sender);
        let frame = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        });
        if let Err(error) = write_frame(&self.stdin, &frame).await {
            self.pending.lock().await.remove(&id);
            return Err(error);
        }
        match tokio::time::timeout(timeout, receiver).await {
            Ok(Ok(Ok(value))) => Ok(value),
            Ok(Ok(Err(message))) => bail!("{method}: {message}"),
            Ok(Err(_)) => {
                self.pending.lock().await.remove(&id);
                bail!("{method}: LSP connection closed")
            }
            Err(_) => {
                self.pending.lock().await.remove(&id);
                bail!("{method}: timed out after {}s", timeout.as_secs())
            }
        }
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        write_frame(
            &self.stdin,
            &json!({"jsonrpc": "2.0", "method": method, "params": params}),
        )
        .await
    }

    async fn open_or_update(&self, path: &Path) -> Result<()> {
        let abs = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        };
        let text = tokio::fs::read_to_string(&abs)
            .await
            .with_context(|| format!("could not read {}", abs.display()))?;
        let uri = file_uri(&abs)?;
        let language_id = language_id(&abs);
        let hash = {
            let mut hasher = DefaultHasher::new();
            text.hash(&mut hasher);
            hasher.finish()
        };
        let mut documents = self.documents.lock().await;
        match documents.get_mut(&uri) {
            None => {
                self.notify(
                    "textDocument/didOpen",
                    json!({
                        "textDocument": {
                            "uri": uri,
                            "languageId": language_id,
                            "version": 1,
                            "text": text
                        }
                    }),
                )
                .await?;
                documents.insert(uri, DocumentState { version: 1, hash });
            }
            Some(state) if state.hash != hash => {
                state.version += 1;
                self.notify(
                    "textDocument/didChange",
                    json!({
                        "textDocument": {"uri": uri, "version": state.version},
                        "contentChanges": [{"text": text}]
                    }),
                )
                .await?;
                state.hash = hash;
            }
            Some(_) => {}
        }
        Ok(())
    }

    async fn position(&self, path: &Path, line: u32, column: u32) -> Result<(String, Value)> {
        let abs = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        };
        let uri = file_uri(&abs)?;
        let text = tokio::fs::read_to_string(&abs).await.unwrap_or_default();
        let line_index = line.saturating_sub(1) as usize;
        let line_text = text.lines().nth(line_index).unwrap_or("");
        let col_index = column.saturating_sub(1) as usize;
        let utf16_column: usize = line_text
            .chars()
            .take(col_index)
            .map(|ch| ch.len_utf16())
            .sum();
        Ok((
            uri,
            json!({
                "line": line_index,
                "character": utf16_column
            }),
        ))
    }

    async fn wait_for_diagnostics(
        &self,
        path: &Path,
        wait: Duration,
    ) -> Result<Vec<LspDiagnostic>> {
        let abs = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        };
        let uri = file_uri(&abs)?;
        tokio::time::sleep(wait).await;
        let diagnostics = self.diagnostics.lock().await;
        Ok(diagnostics.get(&uri).cloned().unwrap_or_default())
    }

    async fn shutdown(&self) {
        let _ = self
            .request("shutdown", Value::Null, Duration::from_secs(3))
            .await;
        let _ = self.notify("exit", Value::Null).await;
        if let Some(handle) = self.reader.lock().await.take() {
            handle.abort();
        }
        if let Ok(mut child) = self._child.try_lock() {
            child.start_kill().ok();
        }
    }
}

async fn reader_loop(
    stdout: tokio::process::ChildStdout,
    stdin: Arc<Mutex<ChildStdin>>,
    pending: Arc<Mutex<HashMap<i64, oneshot::Sender<Result<Value, String>>>>>,
    diagnostics: Arc<Mutex<HashMap<String, Vec<LspDiagnostic>>>>,
    root: PathBuf,
) {
    let mut reader = BufReader::new(stdout);
    loop {
        let message = match read_frame(&mut reader).await {
            Ok(Some(message)) => message,
            Ok(None) => break,
            Err(_) => break,
        };
        if let Some(method) = message.get("method").and_then(Value::as_str) {
            if let Some(id) = message.get("id").cloned() {
                let params = message.get("params").cloned().unwrap_or(Value::Null);
                let response = server_request_response(method, id, &params, &root);
                if let Err(_error) = write_frame(&stdin, &response).await {
                    break;
                }
            } else if method == "textDocument/publishDiagnostics" {
                let params = message.get("params").cloned().unwrap_or(Value::Null);
                if let Some(uri) = params.get("uri").and_then(Value::as_str)
                    && let Some(items) = params.get("diagnostics").and_then(Value::as_array)
                {
                    let parsed: Vec<LspDiagnostic> = items
                        .iter()
                        .filter_map(|item| serde_json::from_value(item.clone()).ok())
                        .collect();
                    diagnostics.lock().await.insert(uri.to_string(), parsed);
                }
            }
            continue;
        }
        let Some(id) = message.get("id").and_then(Value::as_i64) else {
            continue;
        };
        if let Some(sender) = pending.lock().await.remove(&id) {
            if let Some(error) = message.get("error") {
                let message = error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown LSP error")
                    .to_string();
                let _ = sender.send(Err(message));
            } else {
                let _ = sender.send(Ok(message.get("result").cloned().unwrap_or(Value::Null)));
            }
        }
    }
    let mut pending = pending.lock().await;
    for (_, sender) in pending.drain() {
        let _ = sender.send(Err("LSP server closed".into()));
    }
}

fn server_request_response(method: &str, id: Value, params: &Value, root: &Path) -> Value {
    match method {
        "workspace/configuration" => {
            let count = params
                .get("items")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": vec![Value::Null; count]
            })
        }
        "workspace/workspaceFolders" => {
            let uri = file_uri(root).unwrap_or_else(|_| "file:///".to_string());
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": [{"uri": uri, "name": "workspace"}]
            })
        }
        "workspace/applyEdit" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {"applied": false, "failureReason": "Aaykra controls edits"}
        }),
        "client/registerCapability"
        | "client/unregisterCapability"
        | "window/workDoneProgress/create"
        | "window/showMessageRequest" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": Value::Null
        }),
        _ => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": -32601, "message": format!("unsupported server request: {method}")}
        }),
    }
}

async fn read_frame(reader: &mut BufReader<tokio::process::ChildStdout>) -> Result<Option<Value>> {
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        let bytes = reader.read_line(&mut line).await?;
        if bytes == 0 {
            return Ok(None);
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        if let Some(value) = line
            .to_ascii_lowercase()
            .strip_prefix("content-length:")
            .and_then(|value| value.trim().parse::<usize>().ok())
        {
            content_length = Some(value);
        }
    }
    let length = content_length.context("LSP frame missing Content-Length")?;
    if length > 64 * 1024 * 1024 {
        bail!("LSP frame too large ({length} bytes)");
    }
    let mut buffer = vec![0u8; length];
    reader.read_exact(&mut buffer).await?;
    let value = serde_json::from_slice(&buffer).context("invalid LSP JSON")?;
    Ok(Some(value))
}

async fn write_frame(stdin: &Arc<Mutex<ChildStdin>>, value: &Value) -> Result<()> {
    let body = serde_json::to_vec(value)?;
    let mut stdin = stdin.lock().await;
    stdin
        .write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes())
        .await?;
    stdin.write_all(&body).await?;
    stdin.flush().await?;
    Ok(())
}

fn file_uri(path: &Path) -> Result<String> {
    url::Url::from_file_path(path)
        .map(|url| url.to_string())
        .map_err(|()| anyhow::anyhow!("could not convert {} to a file URI", path.display()))
}

fn uri_to_path(uri: &str) -> Option<PathBuf> {
    url::Url::parse(uri).ok()?.to_file_path().ok()
}

fn language_id(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "rs" => "rust",
        "py" | "pyi" => "python",
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" => "javascriptreact",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "typescriptreact",
        "go" => "go",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" | "hh" => "cpp",
        "sh" | "bash" => "shellscript",
        "json" => "json",
        "toml" => "toml",
        "md" => "markdown",
        "html" => "html",
        "css" => "css",
        _ => "plaintext",
    }
}

fn render_locations(result: &Value, root: &Path) -> String {
    let mut locations: Vec<Location> = Vec::new();
    if result.is_null() {
        return "No locations found.".into();
    }
    if let Ok(location) = serde_json::from_value::<Location>(result.clone()) {
        locations.push(location);
    } else if let Some(array) = result.as_array() {
        for item in array {
            if let Ok(location) = serde_json::from_value::<Location>(item.clone()) {
                locations.push(location);
            }
        }
    }
    if locations.is_empty() {
        return "No locations found.".into();
    }
    let mut lines = Vec::new();
    for location in locations.into_iter().take(200) {
        let path = uri_to_path(&location.uri)
            .map(|path| relative_display(&path, root))
            .unwrap_or(location.uri);
        lines.push(format!(
            "{}:{}:{}",
            path,
            location.range.start.line + 1,
            location.range.start.character + 1
        ));
    }
    lines.join("\n")
}

fn render_hover(result: &Value) -> String {
    if result.is_null() {
        return "No hover information.".into();
    }
    let contents = result.get("contents").unwrap_or(result);
    if let Some(text) = contents.as_str() {
        return text.to_string();
    }
    if let Some(text) = contents.get("value").and_then(Value::as_str) {
        return text.to_string();
    }
    if let Some(array) = contents.as_array() {
        let parts: Vec<String> = array
            .iter()
            .filter_map(|item| {
                item.as_str().map(ToOwned::to_owned).or_else(|| {
                    item.get("value")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned)
                })
            })
            .collect();
        if !parts.is_empty() {
            return parts.join("\n\n");
        }
    }
    contents.to_string()
}

#[must_use]
pub fn render_diagnostics(path: &Path, root: &Path, diagnostics: &[LspDiagnostic]) -> String {
    if diagnostics.is_empty() {
        return format!("No diagnostics for {}.", relative_display(path, root));
    }
    let mut lines = vec![format!(
        "{}: {} diagnostic{}",
        relative_display(path, root),
        diagnostics.len(),
        if diagnostics.len() == 1 { "" } else { "s" }
    )];
    let mut sorted = diagnostics.to_vec();
    sorted.sort_by_key(|diagnostic| {
        (
            diagnostic.range.start.line,
            diagnostic.range.start.character,
        )
    });
    for diagnostic in sorted.iter().take(200) {
        let severity = match diagnostic.severity.unwrap_or(3) {
            1 => "error",
            2 => "warning",
            3 => "info",
            4 => "hint",
            _ => "diagnostic",
        };
        let source = diagnostic
            .source
            .as_deref()
            .map(|source| format!(" [{source}]"))
            .unwrap_or_default();
        lines.push(format!(
            "  {}:{}: {}: {}{}",
            diagnostic.range.start.line + 1,
            diagnostic.range.start.character + 1,
            severity,
            diagnostic.message.replace('\n', " "),
            source
        ));
    }
    if sorted.len() > 200 {
        lines.push(format!("  … {} more", sorted.len() - 200));
    }
    lines.join("\n")
}

fn auto_config(extension: &str) -> Option<LspServerConfig> {
    let (candidates, args): (&[&str], &[&str]) = match extension {
        "rs" => (&["rust-analyzer"], &[]),
        "py" | "pyi" => (&["pyright-langserver", "pylsp"], &["--stdio"]),
        "ts" | "tsx" | "mts" | "cts" | "js" | "jsx" | "mjs" | "cjs" => {
            (&["typescript-language-server"], &["--stdio"])
        }
        "go" => (&["gopls"], &[]),
        "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "hh" => (&["clangd"], &[]),
        "sh" | "bash" | "zsh" => (&["bash-language-server"], &["start"]),
        "json" => (&["vscode-json-language-server"], &["--stdio"]),
        "toml" => (&["taplo"], &["lsp", "stdio"]),
        _ => return None,
    };
    for command in candidates {
        if !command_available(command) {
            continue;
        }
        let mut actual_args: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
        if *command == "pylsp" {
            actual_args.clear();
        }
        return Some(LspServerConfig {
            command: (*command).to_string(),
            args: actual_args,
            extensions: vec![extension.to_string()],
            env: IndexMap::new(),
            initialization_options: Value::Null,
            enabled: true,
        });
    }
    None
}

fn command_available(command: &str) -> bool {
    if command.contains('/') {
        return Path::new(command).is_file();
    }
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&paths).any(|dir| {
        let candidate = dir.join(command);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::metadata(&candidate)
                .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
        }
        #[cfg(not(unix))]
        {
            candidate.is_file()
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_diagnostic_notification_shape() {
        let value = json!({
            "range": {"start": {"line": 1, "character": 2}, "end": {"line": 1, "character": 5}},
            "severity": 1,
            "source": "rustc",
            "message": "mismatched types"
        });
        let diagnostic: LspDiagnostic = serde_json::from_value(value).unwrap();
        assert_eq!(diagnostic.range.start.line, 1);
        assert_eq!(diagnostic.message, "mismatched types");
    }

    #[test]
    fn renders_empty_diagnostics() {
        let text = render_diagnostics(Path::new("/p/src/x.rs"), Path::new("/p"), &[]);
        assert!(text.contains("No diagnostics"));
    }

    #[test]
    fn detects_language_ids() {
        assert_eq!(language_id(Path::new("x.rs")), "rust");
        assert_eq!(language_id(Path::new("x.tsx")), "typescriptreact");
    }
}
