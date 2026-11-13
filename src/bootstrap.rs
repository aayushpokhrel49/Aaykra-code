//! Shared construction of configuration, managers, sessions, and runtimes.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};

use crate::agents::AgentRegistry;
use crate::config::{Config, Effort, SandboxMode};
use crate::hooks::HookRunner;
use crate::lsp::LspManager;
use crate::mcp::McpManager;
use crate::models::resolve_model;
use crate::paths::AaykraPaths;
use crate::providers::ProviderRuntime;
use crate::runtime::Runtime;
use crate::session::{Session, SessionStore};
use crate::skills::SkillRegistry;
use crate::types::{PermissionMode, UiGateway};

/// Command-line overrides applied after configuration files are merged. CLI
/// flags win over every configuration layer.
#[derive(Debug, Clone, Default)]
pub struct ConfigOverrides {
    pub effort: Option<Effort>,
    pub max_tokens: Option<u32>,
    pub max_turns: Option<u32>,
    pub permission_mode: Option<PermissionMode>,
    pub sandbox_mode: Option<SandboxMode>,
    pub allow_network: Option<bool>,
}

impl ConfigOverrides {
    fn apply(&self, config: &mut Config) {
        if let Some(effort) = self.effort {
            config.effort = effort;
        }
        if let Some(max_tokens) = self.max_tokens {
            config.max_tokens = max_tokens;
        }
        if let Some(max_turns) = self.max_turns {
            config.max_turns = max_turns;
        }
        if let Some(mode) = self.permission_mode {
            config.permission_mode = mode;
        }
        if let Some(mode) = self.sandbox_mode {
            config.sandbox.mode = mode;
        }
        if let Some(allow) = self.allow_network {
            config.sandbox.allow_network = allow;
        }
    }
}

pub struct Services {
    pub config: Arc<Config>,
    pub paths: AaykraPaths,
    pub cwd: PathBuf,
    pub store: SessionStore,
    pub skills: Arc<SkillRegistry>,
    pub agents: Arc<AgentRegistry>,
    pub hooks: Arc<HookRunner>,
    pub mcp: Arc<McpManager>,
    pub lsp: Arc<LspManager>,
    pub provider_runtime: Arc<ProviderRuntime>,
    pub warnings: Vec<String>,
    pub config_files: Vec<PathBuf>,
}

impl Services {
    pub fn load(cwd: PathBuf, config_path: Option<&Path>) -> Result<Self> {
        Self::load_with(cwd, config_path, ConfigOverrides::default())
    }

    pub fn load_with(
        cwd: PathBuf,
        config_path: Option<&Path>,
        overrides: ConfigOverrides,
    ) -> Result<Self> {
        if !cwd.is_dir() {
            bail!("working directory does not exist: {}", cwd.display());
        }
        let paths = AaykraPaths::discover()?;
        let loaded = Config::load(&cwd, config_path)?;
        let mut config = loaded.config;
        overrides.apply(&mut config);
        let config = Arc::new(config);
        let store = SessionStore::new(paths.clone());
        let provider_runtime = Arc::new(ProviderRuntime::new(config.clone(), paths.clone())?);
        let (skills, mut warnings) = SkillRegistry::discover(&cwd, &paths);
        let (agents, agent_warnings) = AgentRegistry::discover(&cwd, &paths);
        warnings.extend(agent_warnings);
        let hooks = Arc::new(HookRunner::new(config.hooks.clone()));
        let mcp = Arc::new(McpManager::new(
            config.mcp.clone(),
            cwd.clone(),
            provider_runtime.http().clone(),
        ));
        let lsp = Arc::new(LspManager::new(config.lsp.clone(), cwd.clone()));
        Ok(Self {
            config,
            paths,
            cwd,
            store,
            skills: Arc::new(skills),
            agents: Arc::new(agents),
            hooks,
            mcp,
            lsp,
            provider_runtime,
            warnings,
            config_files: loaded.files,
        })
    }

    pub fn runtime(&self, session: Session, ui: Arc<dyn UiGateway>) -> Arc<Runtime> {
        Arc::new(Runtime::new(
            self.config.clone(),
            session,
            self.store.clone(),
            ui,
            self.skills.clone(),
            self.agents.clone(),
            self.hooks.clone(),
            self.mcp.clone(),
            self.lsp.clone(),
            self.provider_runtime.clone(),
            self.cwd.clone(),
            0,
        ))
    }

    pub fn new_session(
        &self,
        provider: Option<&str>,
        model: Option<&str>,
        agent: Option<&str>,
        mode: Option<PermissionMode>,
    ) -> Result<Session> {
        let resolved = resolve_model(&self.config, model, provider)?;
        let mut session = Session::new(
            resolved.reference.provider,
            resolved.reference.model,
            self.cwd.clone(),
        );
        session.permission_mode = mode.unwrap_or(self.config.permission_mode);
        session.agent = self.resolve_agent(agent)?.name.clone();
        Ok(session)
    }

    pub fn resolve_agent(
        &self,
        requested: Option<&str>,
    ) -> Result<&crate::agents::AgentDefinition> {
        let name = requested
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(self.config.default_agent.trim());
        if let Some(agent) = self
            .agents
            .get(if name.is_empty() { "general" } else { name })
        {
            return Ok(agent);
        }
        let available = self
            .agents
            .all()
            .iter()
            .map(|agent| agent.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        bail!("unknown agent `{name}`; available agents: {available}");
    }

    /// Apply CLI overrides to an existing session, validating model and agent.
    pub fn apply_overrides(
        &self,
        session: &mut Session,
        provider: Option<&str>,
        model: Option<&str>,
        agent: Option<&str>,
        mode: Option<PermissionMode>,
    ) -> Result<()> {
        if provider.is_some() || model.is_some() {
            let resolved = resolve_model(&self.config, model, provider)?;
            session.set_model(resolved.reference.provider, resolved.reference.model);
        } else {
            // Validate that a resumed session still points at a usable model.
            let resolved = resolve_model(
                &self.config,
                Some(&session.model),
                Some(&session.provider),
            )
            .with_context(|| {
                format!(
                    "session model {}/{} is no longer configured; pass --model or use /model",
                    session.provider, session.model
                )
            })?;
            session.set_model(resolved.reference.provider, resolved.reference.model);
        }
        if agent.is_some() {
            session.agent = self.resolve_agent(agent)?.name.clone();
        } else if self.agents.get(&session.agent).is_none() {
            session.agent = self.resolve_agent(None)?.name.clone();
        }
        if let Some(mode) = mode {
            session.permission_mode = mode;
        }
        Ok(())
    }

    pub fn load_session(&self, reference: &str) -> Result<Session> {
        self.store.resolve(reference)
    }

    pub fn latest_session(&self) -> Result<Option<Session>> {
        self.store.latest()
    }

    /// The most recent session created for a specific working directory. This
    /// prevents `--continue` from silently resuming another project's session.
    pub fn latest_session_for(&self, cwd: &Path) -> Result<Option<Session>> {
        let canonical = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
        for summary in self.store.list()? {
            let session_cwd = summary
                .cwd
                .canonicalize()
                .unwrap_or_else(|_| summary.cwd.clone());
            if session_cwd == canonical {
                return Ok(Some(self.store.load(&summary.id)?));
            }
        }
        Ok(None)
    }

    pub fn save(&self, session: &Session) -> Result<()> {
        self.store.save(session)
    }
}

/// Resolve a session reference that may be a full id, short id, name, or `"latest"`.
pub fn session_reference(store: &SessionStore, reference: &str) -> Result<Session> {
    if reference.trim().is_empty() || reference.eq_ignore_ascii_case("latest") {
        store
            .latest()?
            .context("no saved sessions yet; start a new session first")
    } else {
        store.resolve(reference)
    }
}
