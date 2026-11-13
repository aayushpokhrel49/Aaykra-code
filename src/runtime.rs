//! Shared runtime services used by agent loops, tools, and UIs.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock as StdRwLock};

use anyhow::{Context, Result, bail};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::agents::AgentRegistry;
use crate::checkpoint::Checkpointer;
use crate::config::{Config, SandboxMode};
use crate::hooks::{HookContext, HookKind, HookOutcome, HookRunner};
use crate::lsp::LspManager;
use crate::mcp::McpManager;
use crate::models::{context_window, resolve_model};
use crate::paths::AaykraPaths;
use crate::permissions::PermissionEngine;
use crate::providers::ProviderRuntime;
use crate::session::{Session, SessionStore};
use crate::skills::SkillRegistry;
use crate::types::{AgentEvent, PermissionDecision, PermissionMode, PermissionRequest, UiGateway};

pub struct Runtime {
    pub config: Arc<Config>,
    pub project_root: PathBuf,
    pub paths: AaykraPaths,
    pub session: Arc<Mutex<Session>>,
    pub store: SessionStore,
    pub ui: Arc<dyn UiGateway>,
    pub permissions: PermissionEngine,
    pub checkpointer: Checkpointer,
    pub skills: Arc<SkillRegistry>,
    pub agents: Arc<AgentRegistry>,
    pub hooks: Arc<HookRunner>,
    pub mcp: Arc<McpManager>,
    pub lsp: Arc<LspManager>,
    pub provider_runtime: Arc<ProviderRuntime>,
    pub cwd: Arc<StdRwLock<PathBuf>>,
    pub cancel: CancellationToken,
    pub agent_depth: usize,
    pub max_agent_depth: usize,
    pub allowed_mcp: Option<Vec<String>>,
    previous_mode: StdRwLock<Option<PermissionMode>>,
}

impl Runtime {
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new(
        config: Arc<Config>,
        session: Session,
        store: SessionStore,
        ui: Arc<dyn UiGateway>,
        skills: Arc<SkillRegistry>,
        agents: Arc<AgentRegistry>,
        hooks: Arc<HookRunner>,
        mcp: Arc<McpManager>,
        lsp: Arc<LspManager>,
        provider_runtime: Arc<ProviderRuntime>,
        project_root: PathBuf,
        agent_depth: usize,
    ) -> Self {
        let paths = store.paths().clone();
        let cwd = session.cwd.clone();
        let mode = session.permission_mode;
        let session = Arc::new(Mutex::new(session));
        let permissions = PermissionEngine::new((*config).clone(), cwd.clone(), mode);
        let checkpointer = Checkpointer::new(paths.checkpoints.clone(), config.checkpoints.enabled);
        Self {
            config,
            project_root,
            paths,
            session,
            store,
            ui,
            permissions,
            checkpointer,
            skills,
            agents,
            hooks,
            mcp,
            lsp,
            provider_runtime,
            cwd: Arc::new(StdRwLock::new(cwd)),
            cancel: CancellationToken::new(),
            agent_depth,
            max_agent_depth: 3,
            allowed_mcp: None,
            previous_mode: StdRwLock::new(None),
        }
    }

    #[must_use]
    pub fn cwd(&self) -> PathBuf {
        self.cwd
            .read()
            .map(|path| path.clone())
            .unwrap_or_else(|_| self.project_root.clone())
    }

    #[must_use]
    pub fn session_id(&self) -> String {
        self.session
            .try_lock()
            .map(|session| session.id.clone())
            .unwrap_or_default()
    }

    pub fn emit(&self, event: AgentEvent) {
        self.ui.emit(event);
    }

    pub async fn session_snapshot(&self) -> Session {
        self.session.lock().await.clone()
    }

    pub async fn save_session(&self) -> Result<()> {
        if !self.config.sessions.auto_save {
            return Ok(());
        }
        let session = self.session.lock().await;
        self.store.save(&session)
    }

    pub async fn set_cwd(&self, cwd: PathBuf) -> Result<()> {
        if !cwd.is_dir() {
            bail!("working directory does not exist: {}", cwd.display());
        }
        if let Ok(mut current) = self.cwd.write() {
            *current = cwd.clone();
        }
        self.permissions.set_cwd(cwd.clone());
        let mut session = self.session.lock().await;
        session.cwd = cwd;
        session.updated_at = chrono::Utc::now();
        self.store.save(&session)?;
        Ok(())
    }

    pub fn set_mode(&self, mode: PermissionMode) {
        {
            let mut previous = self
                .previous_mode
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if mode == PermissionMode::Plan && self.permissions.mode() != PermissionMode::Plan {
                *previous = Some(self.permissions.mode());
            }
        }
        self.permissions.set_mode(mode);
        if let Ok(mut session) = self.session.try_lock() {
            session.permission_mode = mode;
            let _ = self.store.save(&session);
        }
        self.emit(AgentEvent::PermissionModeChanged(mode));
    }

    pub fn enter_plan_mode(&self) -> PermissionMode {
        self.set_mode(PermissionMode::Plan);
        PermissionMode::Plan
    }

    pub fn exit_plan_mode(&self) -> PermissionMode {
        let previous = self
            .previous_mode
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .unwrap_or(PermissionMode::Ask);
        self.set_mode(previous);
        previous
    }

    /// Central authorization. Rules are evaluated first; an `Ask` verdict is
    /// delegated to the active UI and a session-scoped rule can be remembered.
    pub async fn authorize(&self, request: PermissionRequest) -> Result<()> {
        let verdict = self.permissions.evaluate(&request);
        let decision = match verdict {
            crate::permissions::PolicyVerdict::Allow => return Ok(()),
            crate::permissions::PolicyVerdict::Deny(reason) => {
                bail!("permission denied: {reason}")
            }
            crate::permissions::PolicyVerdict::Ask => self.ui.authorize(request).await,
        };
        match decision {
            PermissionDecision::Allow => Ok(()),
            PermissionDecision::AllowForSession { rule } => {
                self.permissions.add_session_rule(&rule);
                Ok(())
            }
            PermissionDecision::Deny { reason } => {
                let reason = reason.unwrap_or_else(|| "user denied the action".into());
                bail!("permission denied: {reason}")
            }
        }
    }

    pub async fn checkpoint(&self, path: &Path) -> Result<()> {
        if !self.checkpointer.enabled() {
            return Ok(());
        }
        let (session_id, turn) = {
            let session = self.session.lock().await;
            (session.id.clone(), session.current_turn)
        };
        if turn == 0 {
            return Ok(());
        }
        self.checkpointer.snapshot(&session_id, turn, path)?;
        Ok(())
    }

    pub async fn record_change(&self, path: &str) {
        let mut session = self.session.lock().await;
        session.record_file_change(path);
        if self.config.sessions.auto_save {
            let _ = self.store.save(&session);
        }
    }

    pub async fn undo_last_turn(&self) -> Result<String> {
        let (session_id, turn) = {
            let session = self.session.lock().await;
            (
                session.id.clone(),
                session.turns.last().map(|turn| turn.number),
            )
        };
        let Some(turn) = turn else {
            return Ok("There is no agent turn to undo.".into());
        };
        let report = self.checkpointer.restore_turn(&session_id, turn)?;
        if !report.errors.is_empty() {
            return Ok(format!(
                "Partial undo for turn {turn}:\n{}\nMessages were kept because restoration failed.",
                report.render()
            ));
        }
        let mut session = self.session.lock().await;
        session.undo_last_turn();
        self.store.save(&session)?;
        Ok(format!(
            "Undid turn {turn}.\n{}\n{} messages remain.",
            report.render(),
            session.messages.len()
        ))
    }

    pub async fn hook_context(&self, kind: HookKind) -> HookContext {
        let session = self.session.lock().await;
        let mut context = HookContext::new(session.id.clone(), self.cwd().display().to_string());
        context.provider = session.provider.clone();
        context.model = session.model.clone();
        context.event = Some(kind);
        context
    }

    pub async fn run_hook(&self, kind: HookKind, context: &HookContext) -> HookOutcome {
        self.hooks.run(kind, context).await
    }

    #[must_use]
    pub fn context_limit(&self) -> usize {
        if self.config.context.window > 0 {
            return self.config.context.window;
        }
        let session_provider;
        let session_model;
        if let Ok(session) = self.session.try_lock() {
            session_provider = session.provider.clone();
            session_model = session.model.clone();
        } else {
            return 128_000;
        }
        context_window(&session_provider, &session_model)
    }

    pub async fn enter_worktree(&self, branch: &str, name: &str) -> Result<PathBuf> {
        let current = self.cwd();
        let repo_root = git_output(&current, &["rev-parse", "--show-toplevel"])
            .await
            .context("worktrees require a git repository")?;
        let repo_root = PathBuf::from(repo_root.trim());
        if self.session.lock().await.worktree.is_some() {
            bail!("already inside an Aaykra worktree; exit it first");
        }
        let safe_name: String = name
            .trim()
            .chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.' {
                    ch
                } else {
                    '-'
                }
            })
            .collect();
        if safe_name.is_empty() || safe_name == "." || safe_name == ".." {
            bail!("invalid worktree name");
        }
        let path = repo_root.join(".aaykra").join("worktrees").join(&safe_name);
        if path.exists() {
            bail!("worktree path already exists: {}", path.display());
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let branch = if branch.trim().is_empty() {
            format!("aaykra/{safe_name}")
        } else {
            branch.trim().to_string()
        };
        let path_string = path.to_string_lossy().to_string();
        let branch_exists = git_output(
            &repo_root,
            &[
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}"),
            ],
        )
        .await
        .is_ok();
        let output = if branch_exists {
            git_output(&repo_root, &["worktree", "add", &path_string, &branch]).await?
        } else {
            git_output(
                &repo_root,
                &["worktree", "add", "-b", &branch, &path_string],
            )
            .await?
        };
        let _ = output;
        {
            let mut session = self.session.lock().await;
            session
                .metadata
                .insert("original_cwd".into(), current.to_string_lossy().to_string());
            session
                .metadata
                .insert("worktree_path".into(), path_string.clone());
            session
                .metadata
                .insert("worktree_branch".into(), branch.clone());
            session.worktree = Some(path.clone());
        }
        self.set_cwd(path.clone()).await?;
        self.emit(AgentEvent::Notice(format!(
            "Entered worktree `{branch}` at {}",
            path.display()
        )));
        Ok(path)
    }

    pub async fn exit_worktree(&self, force: bool, remove: bool) -> Result<PathBuf> {
        let (original, worktree_path, branch) = {
            let session = self.session.lock().await;
            let Some(worktree_path) = session.worktree.clone() else {
                bail!("not inside an Aaykra worktree");
            };
            let original = session
                .metadata
                .get("original_cwd")
                .map(PathBuf::from)
                .unwrap_or_else(|| self.project_root.clone());
            let branch = session
                .metadata
                .get("worktree_branch")
                .cloned()
                .unwrap_or_default();
            (original, worktree_path, branch)
        };
        let current = self.cwd();
        let dirty = git_output(&current, &["status", "--porcelain"])
            .await
            .map(|output| !output.trim().is_empty())
            .unwrap_or(true);
        if dirty && !force {
            bail!(
                "worktree has uncommitted changes; pass force=true to leave anyway (changes stay on disk)"
            );
        }
        self.set_cwd(original.clone()).await?;
        {
            let mut session = self.session.lock().await;
            session.worktree = None;
            session.metadata.remove("worktree_path");
            session.metadata.remove("worktree_branch");
            session.metadata.remove("original_cwd");
        }
        if remove {
            let repo_root = git_output(&original, &["rev-parse", "--show-toplevel"])
                .await
                .context("could not locate git repository")?;
            let path_str = worktree_path.to_string_lossy().to_string();
            let mut args = vec!["worktree", "remove"];
            if force {
                args.push("--force");
            }
            args.push(&path_str);
            git_output(Path::new(repo_root.trim()), &args).await?;
        }
        self.emit(AgentEvent::Notice(format!(
            "Exited worktree{}; returned to {}",
            if branch.is_empty() {
                String::new()
            } else {
                format!(" `{branch}`")
            },
            original.display()
        )));
        Ok(original)
    }

    pub fn sandbox_mode(&self) -> SandboxMode {
        self.config.sandbox.mode
    }
}

pub async fn git_output(cwd: &Path, args: &[&str]) -> Result<String> {
    let output = tokio::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .await
        .with_context(|| format!("could not run git {}", args.join(" ")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "git {} failed: {}",
            args.join(" "),
            stderr.trim().lines().next().unwrap_or("unknown error")
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[must_use]
pub fn resolve_model_for_session(
    config: &Config,
    session: &Session,
) -> Result<crate::models::ResolvedModel> {
    resolve_model(config, Some(&session.model), Some(&session.provider))
}
