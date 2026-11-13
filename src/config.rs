//! Layered configuration.
//!
//! Precedence, lowest to highest:
//! 1. Built-in defaults
//! 2. `~/.aaykra/config.toml`
//! 3. `.aaykra/config.toml`, then `aaykra.toml`
//! 4. An explicit `--config` file
//! 5. Environment variables (`AAYKRA_*`)
//! 6. CLI flags / in-session changes
//!
//! Secrets should normally live in the environment or the mode-0600
//! `~/.aaykra/credentials.toml` store rather than project config.

use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::models::Pricing;
use crate::paths::{
    AaykraPaths, expand_tilde, global_config_path_from_env, project_config_candidates,
};
use crate::types::PermissionMode;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum Protocol {
    #[serde(rename = "anthropic", alias = "claude")]
    Anthropic,
    #[default]
    #[serde(
        rename = "openai",
        alias = "open-ai",
        alias = "openai-compatible",
        alias = "openai_compatible"
    )]
    OpenAi,
    #[serde(rename = "gemini", alias = "google")]
    Gemini,
}

impl Protocol {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAi => "openai",
            Self::Gemini => "gemini",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
#[value(rename_all = "kebab-case")]
pub enum Effort {
    Off,
    Low,
    #[default]
    Medium,
    High,
    Max,
}

impl Effort {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Max => "max",
        }
    }

    #[must_use]
    pub fn anthropic_budget(self) -> u32 {
        match self {
            Self::Off | Self::Low => 2_048,
            Self::Medium => 8_192,
            Self::High => 16_384,
            Self::Max => 31_999,
        }
    }

    #[must_use]
    pub fn openai_reasoning(self) -> Option<&'static str> {
        match self {
            Self::Off => None,
            Self::Low => Some("low"),
            Self::Medium => Some("medium"),
            Self::High | Self::Max => Some("high"),
        }
    }
}

impl std::str::FromStr for Effort {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "off" | "none" | "disabled" => Ok(Self::Off),
            "low" | "min" | "minimal" => Ok(Self::Low),
            "medium" | "med" | "default" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            "max" | "maximum" | "ultra" => Ok(Self::Max),
            other => Err(format!(
                "unknown effort `{other}` (off, low, medium, high, max)"
            )),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    #[serde(default)]
    pub protocol: Protocol,
    #[serde(default)]
    pub base_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub known_models: Vec<String>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub headers: IndexMap<String, String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub requires_key: bool,
    /// Override the request field used for the output token limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens_field: Option<String>,
    /// Extra JSON fields merged into each request body.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub extra_body: IndexMap<String, Value>,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            protocol: Protocol::OpenAi,
            base_url: String::new(),
            api_key: None,
            api_key_env: None,
            default_model: None,
            known_models: Vec::new(),
            headers: IndexMap::new(),
            enabled: true,
            requires_key: true,
            max_tokens_field: None,
            extra_body: IndexMap::new(),
        }
    }
}

impl fmt::Debug for ProviderConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderConfig")
            .field("protocol", &self.protocol)
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "[redacted]"))
            .field("api_key_env", &self.api_key_env)
            .field("default_model", &self.default_model)
            .field("enabled", &self.enabled)
            .field("requires_key", &self.requires_key)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct PermissionRules {
    pub allow: Vec<String>,
    pub ask: Vec<String>,
    pub deny: Vec<String>,
    /// Extra directories the agent may access without an out-of-project warning.
    pub additional_directories: Vec<PathBuf>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
#[value(rename_all = "kebab-case")]
pub enum SandboxMode {
    Off,
    #[default]
    Auto,
    Bubblewrap,
    Seatbelt,
}

impl SandboxMode {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Auto => "auto",
            Self::Bubblewrap => "bubblewrap",
            Self::Seatbelt => "seatbelt",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SandboxConfig {
    pub mode: SandboxMode,
    pub allow_network: bool,
    pub extra_writable: Vec<PathBuf>,
    pub extra_readonly: Vec<PathBuf>,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            mode: SandboxMode::Auto,
            allow_network: true,
            extra_writable: Vec::new(),
            extra_readonly: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ContextConfig {
    /// Soft context window in tokens. 0 means infer from the model.
    pub window: usize,
    /// Fraction of the window at which automatic compaction runs.
    pub compact_threshold: f32,
    pub auto_compact: bool,
    /// Refresh codebase context and condense older work when a session is
    /// resumed or the active model changes.
    pub resume_handoff: bool,
    pub max_tool_output_chars: usize,
    pub max_tool_output_lines: usize,
    pub repo_map_files: usize,
    /// Reuse a repository overview cache for this many seconds. 0 disables it.
    pub repo_map_cache_secs: u64,
    pub auto_diagnostics: bool,
    pub deferred_mcp_tools: bool,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            window: 0,
            compact_threshold: 0.82,
            auto_compact: true,
            resume_handoff: true,
            max_tool_output_chars: 60_000,
            max_tool_output_lines: 2_000,
            repo_map_files: 1_500,
            repo_map_cache_secs: 300,
            auto_diagnostics: true,
            deferred_mcp_tools: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    pub show_thinking: bool,
    pub show_tool_output: bool,
    pub compact_tools: bool,
    pub mouse: bool,
    pub animations: bool,
    pub theme: String,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            show_thinking: false,
            show_tool_output: false,
            compact_tools: true,
            mouse: true,
            animations: true,
            theme: "aaykra".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CheckpointConfig {
    pub enabled: bool,
    pub keep_turns: usize,
}

impl Default for CheckpointConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            keep_turns: 50,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionsConfig {
    pub retention_days: u64,
    pub auto_save: bool,
}

impl Default for SessionsConfig {
    fn default() -> Self {
        Self {
            retention_days: 90,
            auto_save: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WebConfig {
    pub search_provider: String,
    pub tavily_api_key: Option<String>,
    pub brave_api_key: Option<String>,
    pub max_results: usize,
    pub fetch_max_bytes: usize,
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            search_provider: "duckduckgo".into(),
            tavily_api_key: None,
            brave_api_key: None,
            max_results: 8,
            fetch_max_bytes: 2_000_000,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct McpServerConfig {
    pub command: Option<String>,
    pub args: Vec<String>,
    pub env: IndexMap<String, String>,
    pub url: Option<String>,
    pub headers: IndexMap<String, String>,
    pub enabled: bool,
    pub timeout_ms: u64,
}

impl Default for McpServerConfig {
    fn default() -> Self {
        Self {
            command: None,
            args: Vec::new(),
            env: IndexMap::new(),
            url: None,
            headers: IndexMap::new(),
            enabled: true,
            timeout_ms: 20_000,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LspServerConfig {
    pub command: String,
    pub args: Vec<String>,
    pub extensions: Vec<String>,
    pub env: IndexMap<String, String>,
    pub initialization_options: Value,
    pub enabled: bool,
}

impl Default for LspServerConfig {
    fn default() -> Self {
        Self {
            command: String::new(),
            args: Vec::new(),
            extensions: Vec::new(),
            env: IndexMap::new(),
            initialization_options: Value::Null,
            enabled: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct HookConfig {
    pub command: String,
    /// Regex matched against the tool name (PreToolUse/PostToolUse) or prompt.
    pub matcher: Option<String>,
    pub timeout_ms: u64,
    /// A nonzero blocking hook prevents the action.
    pub blocking: bool,
}

impl Default for HookConfig {
    fn default() -> Self {
        Self {
            command: String::new(),
            matcher: None,
            timeout_ms: 30_000,
            blocking: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GitConfig {
    pub auto_commit: bool,
    pub commit_style: String,
    pub create_pr: bool,
    pub protected_branches: Vec<String>,
}

impl Default for GitConfig {
    fn default() -> Self {
        Self {
            auto_commit: false,
            commit_style: "conventional".into(),
            create_pr: false,
            protected_branches: vec!["main".into(), "master".into()],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub default_provider: Option<String>,
    pub model: Option<String>,
    pub fallback_models: Vec<String>,
    pub max_tokens: u32,
    pub temperature: Option<f32>,
    pub effort: Effort,
    pub max_turns: u32,
    pub default_agent: String,
    pub auto_memory: bool,
    pub instructions: Vec<PathBuf>,
    pub disabled_tools: Vec<String>,
    pub providers: IndexMap<String, ProviderConfig>,
    pub permission_mode: PermissionMode,
    pub permissions: PermissionRules,
    pub sandbox: SandboxConfig,
    pub context: ContextConfig,
    pub ui: UiConfig,
    pub checkpoints: CheckpointConfig,
    pub sessions: SessionsConfig,
    pub web: WebConfig,
    pub git: GitConfig,
    pub mcp: IndexMap<String, McpServerConfig>,
    pub lsp: IndexMap<String, LspServerConfig>,
    pub hooks: IndexMap<String, Vec<HookConfig>>,
    pub pricing: IndexMap<String, Pricing>,
}

impl Default for Config {
    fn default() -> Self {
        let mut providers = IndexMap::new();
        providers.insert(
            "anthropic".into(),
            ProviderConfig {
                protocol: Protocol::Anthropic,
                base_url: "https://api.anthropic.com".into(),
                api_key_env: Some("ANTHROPIC_API_KEY".into()),
                default_model: Some("claude-sonnet-4-5".into()),
                known_models: vec![
                    "claude-opus-4-1".into(),
                    "claude-sonnet-4-5".into(),
                    "claude-haiku-4-5".into(),
                ],
                ..ProviderConfig::default()
            },
        );
        providers.insert(
            "openai".into(),
            ProviderConfig {
                base_url: "https://api.openai.com/v1".into(),
                api_key_env: Some("OPENAI_API_KEY".into()),
                default_model: Some("gpt-5.1".into()),
                known_models: vec!["gpt-5.1".into(), "gpt-5".into(), "gpt-4.1".into()],
                ..ProviderConfig::default()
            },
        );
        providers.insert(
            "deepseek".into(),
            ProviderConfig {
                base_url: "https://api.deepseek.com/v1".into(),
                api_key_env: Some("DEEPSEEK_API_KEY".into()),
                default_model: Some("deepseek-chat".into()),
                known_models: vec!["deepseek-chat".into(), "deepseek-reasoner".into()],
                ..ProviderConfig::default()
            },
        );
        providers.insert(
            "groq".into(),
            ProviderConfig {
                base_url: "https://api.groq.com/openai/v1".into(),
                api_key_env: Some("GROQ_API_KEY".into()),
                default_model: Some("llama-3.3-70b-versatile".into()),
                ..ProviderConfig::default()
            },
        );
        providers.insert(
            "gemini".into(),
            ProviderConfig {
                protocol: Protocol::Gemini,
                base_url: "https://generativelanguage.googleapis.com/v1beta".into(),
                api_key_env: Some("GEMINI_API_KEY".into()),
                default_model: Some("gemini-2.5-pro".into()),
                known_models: vec!["gemini-2.5-pro".into(), "gemini-2.5-flash".into()],
                ..ProviderConfig::default()
            },
        );
        providers.insert(
            "openrouter".into(),
            ProviderConfig {
                base_url: "https://openrouter.ai/api/v1".into(),
                api_key_env: Some("OPENROUTER_API_KEY".into()),
                default_model: Some("anthropic/claude-sonnet-4.5".into()),
                ..ProviderConfig::default()
            },
        );
        providers.insert(
            "together".into(),
            ProviderConfig {
                base_url: "https://api.together.xyz/v1".into(),
                api_key_env: Some("TOGETHER_API_KEY".into()),
                default_model: Some("meta-llama/Llama-3.3-70B-Instruct-Turbo".into()),
                ..ProviderConfig::default()
            },
        );
        providers.insert(
            "fireworks".into(),
            ProviderConfig {
                base_url: "https://api.fireworks.ai/inference/v1".into(),
                api_key_env: Some("FIREWORKS_API_KEY".into()),
                ..ProviderConfig::default()
            },
        );
        providers.insert(
            "mistral".into(),
            ProviderConfig {
                base_url: "https://api.mistral.ai/v1".into(),
                api_key_env: Some("MISTRAL_API_KEY".into()),
                default_model: Some("mistral-large-latest".into()),
                ..ProviderConfig::default()
            },
        );
        providers.insert(
            "ollama".into(),
            ProviderConfig {
                base_url: "http://localhost:11434/v1".into(),
                default_model: Some("qwen2.5-coder:7b".into()),
                requires_key: false,
                ..ProviderConfig::default()
            },
        );
        providers.insert(
            "lmstudio".into(),
            ProviderConfig {
                base_url: "http://localhost:1234/v1".into(),
                default_model: Some("local-model".into()),
                requires_key: false,
                ..ProviderConfig::default()
            },
        );
        providers.insert(
            "vllm".into(),
            ProviderConfig {
                base_url: "http://localhost:8000/v1".into(),
                requires_key: false,
                ..ProviderConfig::default()
            },
        );
        providers.insert(
            "llamacpp".into(),
            ProviderConfig {
                base_url: "http://localhost:8080/v1".into(),
                requires_key: false,
                ..ProviderConfig::default()
            },
        );
        providers.insert(
            "llamafile".into(),
            ProviderConfig {
                base_url: "http://localhost:8080/v1".into(),
                requires_key: false,
                ..ProviderConfig::default()
            },
        );
        Self {
            default_provider: None,
            model: None,
            fallback_models: Vec::new(),
            max_tokens: 8192,
            temperature: None,
            effort: Effort::Medium,
            max_turns: 100,
            default_agent: "general".into(),
            auto_memory: true,
            instructions: Vec::new(),
            disabled_tools: Vec::new(),
            providers,
            permission_mode: PermissionMode::Ask,
            permissions: PermissionRules::default(),
            sandbox: SandboxConfig::default(),
            context: ContextConfig::default(),
            ui: UiConfig::default(),
            checkpoints: CheckpointConfig::default(),
            sessions: SessionsConfig::default(),
            web: WebConfig::default(),
            git: GitConfig::default(),
            mcp: IndexMap::new(),
            lsp: IndexMap::new(),
            hooks: IndexMap::new(),
            pricing: IndexMap::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct LoadedConfig {
    pub config: Config,
    pub files: Vec<PathBuf>,
}

impl Config {
    /// Load the layered configuration for a project.
    pub fn load(project_root: &Path, explicit: Option<&Path>) -> Result<LoadedConfig> {
        let mut base = serde_json::to_value(Self::default())
            .context("could not serialize default configuration")?;

        let mut files = Vec::new();
        let global = global_config_path_from_env()?;
        for path in std::iter::once(global)
            .chain(project_config_candidates(project_root))
            .chain(explicit.map(Path::to_path_buf))
        {
            if !path.exists() {
                if explicit.is_some_and(|p| p == path) {
                    bail!("config file not found: {}", path.display());
                }
                continue;
            }
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("could not read {}", path.display()))?;
            let overlay: toml::Value = toml::from_str(&text)
                .with_context(|| format!("invalid TOML in {}", path.display()))?;
            let overlay = serde_json::to_value(overlay)
                .with_context(|| format!("could not normalize {}", path.display()))?;
            deep_merge(&mut base, overlay);
            files.push(path);
        }

        // Environment variables override files. CLI flags are applied on top of
        // the loaded configuration by `bootstrap::ConfigOverrides`.
        apply_env_overlay(&mut base)?;

        let config: Self = serde_json::from_value(base).context("invalid Aaykra configuration")?;
        config.validate()?;
        Ok(LoadedConfig { config, files })
    }

    fn validate(&self) -> Result<()> {
        if self.max_tokens == 0 {
            bail!("max_tokens must be greater than zero");
        }
        if self.max_turns == 0 {
            bail!("max_turns must be greater than zero");
        }
        if !(0.1..=1.0).contains(&self.context.compact_threshold) {
            bail!("context.compact_threshold must be between 0.1 and 1.0");
        }
        for (name, provider) in &self.providers {
            if provider.enabled && provider.base_url.trim().is_empty() {
                bail!("provider `{name}` has an empty base_url");
            }
        }
        for (name, server) in &self.mcp {
            if server.enabled && server.command.is_none() && server.url.is_none() {
                bail!("MCP server `{name}` needs either command or url");
            }
        }
        Ok(())
    }

    #[must_use]
    pub fn provider(&self, name: &str) -> Option<&ProviderConfig> {
        self.providers.get(name)
    }

    #[must_use]
    pub fn enabled_providers(&self) -> impl Iterator<Item = (&str, &ProviderConfig)> {
        self.providers
            .iter()
            .filter(|(_, config)| config.enabled)
            .map(|(name, config)| (name.as_str(), config))
    }

    #[must_use]
    pub fn config_template() -> String {
        let template = r#"# Aaykra Code configuration
# Docs: https://github.com/aaykra/aaykra-code

# default_provider = "anthropic"
# model = "anthropic/claude-sonnet-4-5"
# fallback_models = ["openai/gpt-5.1", "deepseek/deepseek-chat"]
# max_tokens = 8192
# effort = "medium"           # off | low | medium | high | max
# permission_mode = "ask"     # ask | accept-edits | plan | auto | bypass

[permissions]
# Rules are `Tool` or `Tool:glob-pattern`.
allow = ["Glob", "Grep", "Read"]
ask = ["Bash(git push:*)"]
deny = ["Bash(rm -rf /:*)"]
additional_directories = []

[sandbox]
mode = "auto"                # off | auto | bubblewrap | seatbelt
allow_network = true

[context]
auto_compact = true
compact_threshold = 0.82
resume_handoff = true         # compact old work + refresh repo map on resume/model switch
repo_map_files = 1500
repo_map_cache_secs = 300     # 0 disables the repository map cache
auto_diagnostics = true
deferred_mcp_tools = true

[ui]
show_thinking = false
compact_tools = true

[web]
search_provider = "duckduckgo"  # duckduckgo | tavily | brave

# --- Providers -----------------------------------------------------------
# Keys are read from the environment by default. Prefer `akc auth set`.
# [providers.openai]
# base_url = "https://api.openai.com/v1"
# default_model = "gpt-5.1"
#
# [providers.local]
# protocol = "openai"
# base_url = "http://localhost:8080/v1"
# default_model = "qwen3-coder"
# requires_key = false

# --- MCP servers ---------------------------------------------------------
# [mcp.filesystem]
# command = "npx"
# args = ["-y", "@modelcontextprotocol/server-filesystem", "."]

# [mcp.remote]
# url = "https://mcp.example.com/mcp"
# headers = { Authorization = "Bearer ..." }

# --- LSP servers ---------------------------------------------------------
# [lsp.rust]
# command = "rust-analyzer"
# extensions = ["rs"]

# --- Hooks ---------------------------------------------------------------
# [hooks.pre_tool_use]
# [[hooks.pre_tool_use]]
# command = "echo \"$AAYKRA_TOOL_NAME\" >> .aaykra/tool.log"
# matcher = "Bash|Write|Edit"
# timeout_ms = 10000
# blocking = true
"#;
        template.to_string()
    }

    #[must_use]
    pub fn instructions_paths(&self, project_root: &Path, paths: &AaykraPaths) -> Vec<PathBuf> {
        let mut result = Vec::new();
        for candidate in [
            project_root.join("AAYKRA.md"),
            project_root.join("CLAUDE.md"),
            project_root.join("AGENTS.md"),
        ] {
            if candidate.is_file() {
                result.push(candidate);
                break;
            }
        }
        let global = paths.global_instructions();
        if global.is_file() {
            result.push(global);
        }
        for extra in &self.instructions {
            let path = if extra.is_absolute() {
                extra.clone()
            } else {
                project_root.join(extra)
            };
            if path.is_file() {
                result.push(path);
            }
        }
        result
    }
}

fn default_true() -> bool {
    true
}

fn deep_merge(base: &mut Value, overlay: Value) {
    match (base, overlay) {
        (Value::Object(base), Value::Object(overlay)) => {
            for (key, value) in overlay {
                if let Some(base_value) = base.get_mut(&key) {
                    deep_merge(base_value, value);
                } else {
                    base.insert(key, value);
                }
            }
        }
        (base, overlay) => *base = overlay,
    }
}

fn set_json_path(root: &mut Value, path: &[&str], value: Value) {
    let mut current = root;
    for segment in &path[..path.len().saturating_sub(1)] {
        if !current.get(*segment).is_some_and(Value::is_object) {
            let Some(map) = current.as_object_mut() else {
                return;
            };
            map.insert(
                (*segment).to_string(),
                Value::Object(serde_json::Map::new()),
            );
        }
        let Some(next) = current.get_mut(*segment) else {
            return;
        };
        current = next;
    }
    if let Some(last) = path.last()
        && let Some(map) = current.as_object_mut()
    {
        map.insert((*last).to_string(), value);
    }
}

fn env_any(names: &[&str]) -> Option<String> {
    names
        .iter()
        .find_map(|name| std::env::var(name).ok().filter(|v| !v.trim().is_empty()))
}

fn apply_env_overlay(root: &mut Value) -> Result<()> {
    // Global behavior.
    for (names, key) in [
        (&["AAYKRA_MODEL", "AKC_MODEL"][..], &["model"][..]),
        (
            &["AAYKRA_PROVIDER", "AKC_PROVIDER"][..],
            &["default_provider"][..],
        ),
        (
            &["AAYKRA_PERMISSION_MODE", "AKC_PERMISSION_MODE"][..],
            &["permission_mode"][..],
        ),
        (
            &["AAYKRA_MAX_TOKENS", "AKC_MAX_TOKENS"][..],
            &["max_tokens"][..],
        ),
        (&["AAYKRA_EFFORT", "AKC_EFFORT"][..], &["effort"][..]),
    ] {
        if let Some(value) = env_any(names) {
            let parsed = if key[0] == "max_tokens" {
                match value.parse::<u64>() {
                    Ok(number) => Value::from(number),
                    Err(_) => bail!("{names:?} must be an integer, got `{value}`"),
                }
            } else {
                Value::String(value)
            };
            set_json_path(root, key, parsed);
        }
    }

    if let Some(value) = env_any(&["AAYKRA_FALLBACK_MODELS", "AKC_FALLBACK_MODELS"]) {
        let models = value
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| Value::String(s.to_string()))
            .collect();
        set_json_path(root, &["fallback_models"], Value::Array(models));
    }

    // Provider credentials and endpoints.
    let env_providers: &[(&str, &str, &[&str], &[&str])] = &[
        (
            "anthropic",
            "api_key",
            &["ANTHROPIC_API_KEY"],
            &["ANTHROPIC_BASE_URL"],
        ),
        (
            "openai",
            "api_key",
            &["OPENAI_API_KEY"],
            &["OPENAI_BASE_URL"],
        ),
        (
            "deepseek",
            "api_key",
            &["DEEPSEEK_API_KEY"],
            &["DEEPSEEK_BASE_URL"],
        ),
        ("groq", "api_key", &["GROQ_API_KEY"], &["GROQ_BASE_URL"]),
        (
            "gemini",
            "api_key",
            &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
            &["GEMINI_BASE_URL"],
        ),
        (
            "openrouter",
            "api_key",
            &["OPENROUTER_API_KEY"],
            &["OPENROUTER_BASE_URL"],
        ),
        (
            "together",
            "api_key",
            &["TOGETHER_API_KEY"],
            &["TOGETHER_BASE_URL"],
        ),
        (
            "fireworks",
            "api_key",
            &["FIREWORKS_API_KEY"],
            &["FIREWORKS_BASE_URL"],
        ),
        (
            "mistral",
            "api_key",
            &["MISTRAL_API_KEY"],
            &["MISTRAL_BASE_URL"],
        ),
    ];
    for (provider, field, key_vars, base_vars) in env_providers {
        if let Some(value) = env_any(key_vars) {
            set_json_path(root, &["providers", provider, field], Value::String(value));
        }
        if let Some(value) = env_any(base_vars) {
            set_json_path(
                root,
                &["providers", provider, "base_url"],
                Value::String(value),
            );
        }
    }
    Ok(())
}

pub fn merge_toml_value(base: &mut toml::Value, overlay: toml::Value) {
    match (base, overlay) {
        (toml::Value::Table(base), toml::Value::Table(overlay)) => {
            for (key, value) in overlay {
                if let Some(base_value) = base.get_mut(&key) {
                    merge_toml_value(base_value, value);
                } else {
                    base.insert(key, value);
                }
            }
        }
        (base, overlay) => *base = overlay,
    }
}

#[must_use]
pub fn expand_path(value: &Path) -> PathBuf {
    if let Some(s) = value.to_str() {
        expand_tilde(s)
    } else {
        value.to_path_buf()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_include_local_providers_without_keys() {
        let config = Config::default();
        assert!(!config.providers["ollama"].requires_key);
        assert_eq!(config.providers["anthropic"].protocol, Protocol::Anthropic);
    }

    #[test]
    fn deep_merge_preserves_defaults() {
        let mut base = serde_json::to_value(Config::default()).unwrap();
        let overlay: Value =
            serde_json::from_str(r#"{"providers":{"openai":{"default_model":"custom"}}}"#).unwrap();
        deep_merge(&mut base, overlay);
        let config: Config = serde_json::from_value(base).unwrap();
        assert_eq!(
            config.providers["openai"].default_model.as_deref(),
            Some("custom")
        );
        assert_eq!(
            config.providers["openai"].base_url,
            "https://api.openai.com/v1"
        );
    }

    #[test]
    fn protocol_names_match_documentation_and_legacy_spelling() {
        let mut base = serde_json::to_value(Config::default()).unwrap();
        deep_merge(
            &mut base,
            serde_json::json!({"providers": {"custom": {"protocol": "openai"}}}),
        );
        let config: Config = serde_json::from_value(base).unwrap();
        assert_eq!(config.providers["custom"].protocol, Protocol::OpenAi);

        let legacy: Protocol = serde_json::from_str("\"open-ai\"").unwrap();
        assert_eq!(legacy, Protocol::OpenAi);
        assert_eq!(
            serde_json::to_string(&Protocol::OpenAi).unwrap(),
            "\"openai\""
        );
    }

    #[test]
    fn unknown_effort_is_rejected() {
        assert!("extreme".parse::<Effort>().is_err());
    }

    #[test]
    fn config_deserializes_layered_values() {
        let mut base = serde_json::to_value(Config::default()).unwrap();
        deep_merge(
            &mut base,
            serde_json::json!({
                "permission_mode": "plan",
                "context": {"auto_compact": false},
                "mcp": {"test": {"command": "true"}}
            }),
        );
        let config: Config = serde_json::from_value(base).unwrap();
        assert_eq!(config.permission_mode, PermissionMode::Plan);
        assert!(!config.context.auto_compact);
        assert!(config.mcp.contains_key("test"));
    }
}
