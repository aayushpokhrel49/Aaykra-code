//! Slash commands shared by the interactive TUI, headless mode, and the server.
//!
//! Commands return a [`CommandAction`] so every front-end can decide how to
//! present a reply or whether the text should be sent to the model.

use std::sync::Arc;

use crate::agent::{compact_session, context_report};
use crate::memory::{MemoryScope, MemoryStore};
use crate::models::{format_cost, resolve_model};
use crate::providers::all_known_models;
use crate::runtime::{Runtime, git_output};
use crate::session::SessionStore;
use crate::types::{AgentEvent, PermissionMode, compact_number};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandAction {
    /// The input did not begin with a slash command; send it to the model.
    NotACommand,
    /// Handled locally; show this text to the user.
    Reply(String),
    /// Handled locally, but ask the model with this prompt.
    RunPrompt(String),
    /// Leave the current interface.
    Quit,
    /// Wipe the visible transcript. The session was already reset on disk.
    Clear,
}

impl CommandAction {
    #[must_use]
    pub fn reply(text: impl Into<String>) -> Self {
        Self::Reply(text.into())
    }
}

#[must_use]
pub fn is_command(input: &str) -> bool {
    let trimmed = input.trim_start();
    trimmed.starts_with('/') && !trimmed.starts_with("//")
}

/// Parse and execute a slash command. Unknown commands return a helpful reply
/// instead of being sent to the provider.
pub async fn handle(runtime: Arc<Runtime>, input: &str) -> CommandAction {
    let trimmed = input.trim();
    let Some(rest) = trimmed.strip_prefix('/') else {
        return CommandAction::NotACommand;
    };
    if rest.starts_with('/') {
        return CommandAction::NotACommand;
    }
    let (name, args) = match rest.split_once(char::is_whitespace) {
        Some((name, args)) => (name.to_ascii_lowercase(), args.trim()),
        None => (rest.to_ascii_lowercase(), ""),
    };
    let args = args.trim();
    match name.as_str() {
        "" => CommandAction::reply(help_text()),
        "help" | "?" => CommandAction::reply(help_text()),
        "quit" | "exit" | "q" => CommandAction::Quit,
        "clear" => clear(&runtime).await,
        "model" => model(&runtime, args).await,
        "plan" => plan(&runtime, args),
        "compact" => compact(&runtime).await,
        "context" | "tokens" => CommandAction::reply(context_report(&runtime).await),
        "mcp" => mcp(&runtime, args).await,
        "agents" | "agent" => agent_catalog(&runtime, args),
        "skills" | "skill" => skills_catalog(&runtime, args),
        "memory" | "memories" => memory(&runtime, args),
        "diff" => diff(&runtime, args).await,
        "code-review" | "review" => code_review(&runtime, args).await,
        "init" => init(&runtime, args),
        "undo" => undo(&runtime).await,
        "permissions" | "permission" => permissions(&runtime),
        "status" => status(&runtime).await,
        "name" | "rename" => session_name(&runtime, args).await,
        "sessions" | "resume" => sessions(&runtime, args),
        "worktree" => worktree(&runtime, args).await,
        "version" => CommandAction::reply(format!("aaykra-code {}", env!("CARGO_PKG_VERSION"))),
        other => CommandAction::reply(format!(
            "Unknown command `/{other}`. Type `/help` for the command list."
        )),
    }
}

fn help_text() -> String {
    let commands = [
        ("/help", "show this help"),
        (
            "/model [model|provider model]",
            "list models or switch the active model",
        ),
        ("/plan [on|off]", "toggle read-only plan mode"),
        ("/compact", "summarize older messages to reclaim context"),
        ("/context", "show context usage and session statistics"),
        (
            "/mcp [status|discover|server…]",
            "MCP status or discover tools/resources",
        ),
        ("/agents [name]", "list agents or switch the active agent"),
        ("/skills", "list available skills"),
        (
            "/memory [add|add-global|remove] [text]",
            "show or edit persistent memory",
        ),
        ("/diff [path]", "show git status and the working-tree diff"),
        (
            "/code-review [focus]",
            "start a code review of the current changes",
        ),
        ("/init", "generate AAYKRA.md project instructions"),
        (
            "/undo",
            "restore files and conversation to before the last turn",
        ),
        (
            "/clear",
            "reset the conversation (checkpoints are preserved)",
        ),
        ("/permissions", "show permission rules, mode, and sandbox"),
        ("/status", "show session, model, usage, and context status"),
        ("/name [name|none]", "show or set the session name"),
        (
            "/sessions",
            "list saved sessions (resume with akc --resume)",
        ),
        (
            "/worktree [status|enter|exit]",
            "manage a parallel git worktree",
        ),
        ("/quit", "exit Aaykra Code"),
    ];
    let mut out = String::from("Aaykra Code commands:\n");
    for (name, description) in commands {
        out.push_str(&format!("  {name:<34} {description}\n"));
    }
    out.push_str(
        "\nTUI keys: Enter send · Shift+Enter newline · Shift+Tab cycle mode · Ctrl+C cancel · Ctrl+D quit\n\
         Prefix a message with // to send a literal leading slash.",
    );
    out
}

async fn clear(runtime: &Arc<Runtime>) -> CommandAction {
    {
        let mut session = runtime.session.lock().await;
        session.messages.clear();
        session.turns.clear();
        session.todos.clear();
        session.files_changed.clear();
        session.usage = crate::types::Usage::default();
        session.cost_usd = 0.0;
        session.last_stop_reason = None;
        session.updated_at = chrono::Utc::now();
        // Keep `current_turn` so checkpoint manifests from earlier turns cannot
        // collide with the fresh conversation.
    }
    if let Err(error) = runtime.save_session().await {
        return CommandAction::reply(format!("Cleared, but could not save: {error:#}"));
    }
    CommandAction::Clear
}

async fn model(runtime: &Arc<Runtime>, args: &str) -> CommandAction {
    let session = runtime.session_snapshot().await;
    if args.is_empty() || matches!(args, "list" | "ls" | "?") {
        let mut out = format!("Current model: {}/{}\n", session.provider, session.model);
        if !runtime.config.fallback_models.is_empty() {
            out.push_str(&format!(
                "Fallbacks: {}\n",
                runtime.config.fallback_models.join(", ")
            ));
        }
        out.push('\n');
        let known = all_known_models(&runtime.config);
        for (provider, models) in known {
            let Some(config) = runtime.config.provider(&provider) else {
                continue;
            };
            if !config.enabled {
                continue;
            }
            let key = if runtime.provider_runtime.has_key(&provider) {
                "ready"
            } else {
                "missing key"
            };
            out.push_str(&format!("{provider}  ({key})\n"));
            if models.is_empty() {
                out.push_str("  (no known models; pass any model id)\n");
                continue;
            }
            for candidate in models {
                let marker = if provider == session.provider && candidate == session.model {
                    "*"
                } else {
                    " "
                };
                out.push_str(&format!("  {marker} {candidate}\n"));
            }
        }
        out.push_str("\nSwitch with `/model <model>` or `/model <provider> <model>`.");
        return CommandAction::reply(out);
    }

    let (provider, model_name) = parse_model_selector(runtime, args);
    let resolved = match resolve_model(&runtime.config, Some(model_name), provider.as_deref()) {
        Ok(resolved) => resolved,
        Err(error) => return CommandAction::reply(format!("Could not switch model: {error:#}")),
    };
    let changed = {
        let mut state = runtime.session.lock().await;
        state.set_model(
            resolved.reference.provider.clone(),
            resolved.reference.model.clone(),
        )
    };
    if let Err(error) = runtime.save_session().await {
        return CommandAction::reply(format!(
            "Switched to {}/{}, but could not save: {error:#}",
            resolved.reference.provider, resolved.reference.model
        ));
    }
    runtime.emit(AgentEvent::ModelChanged {
        provider: resolved.reference.provider.clone(),
        model: resolved.reference.model.clone(),
    });
    CommandAction::reply(format!(
        "Model set to {}/{}.{} The next turn refreshes codebase context and resumes from the current conversation.",
        resolved.reference.provider,
        resolved.reference.model,
        if changed { "" } else { " Already active." }
    ))
}

fn parse_model_selector<'a>(runtime: &Runtime, args: &'a str) -> (Option<String>, &'a str) {
    if let Some((first, rest)) = args.split_once(char::is_whitespace) {
        let rest = rest.trim();
        if !rest.is_empty() && runtime.config.provider(first).is_some() {
            return (Some(first.to_string()), rest);
        }
    }
    (None, args)
}

fn plan(runtime: &Arc<Runtime>, args: &str) -> CommandAction {
    let current = runtime.permissions.mode();
    let target = match args.to_ascii_lowercase().as_str() {
        "" => {
            if current == PermissionMode::Plan {
                None
            } else {
                Some(PermissionMode::Plan)
            }
        }
        "on" | "enter" | "plan" => Some(PermissionMode::Plan),
        "off" | "exit" | "build" => {
            if current == PermissionMode::Plan {
                None
            } else {
                Some(PermissionMode::Ask)
            }
        }
        other => {
            return CommandAction::reply(format!(
                "Unknown plan argument `{other}`. Use `/plan on` or `/plan off`."
            ));
        }
    };
    let mode = match target {
        Some(PermissionMode::Plan) => runtime.enter_plan_mode(),
        Some(mode) => {
            runtime.set_mode(mode);
            mode
        }
        None => runtime.exit_plan_mode(),
    };
    let suffix = if mode == PermissionMode::Plan {
        "Read-only: edits, destructive shell, and mutating tools are blocked."
    } else {
        "Writes are available again."
    };
    CommandAction::reply(format!(
        "Permission mode: {} ({}). {suffix}",
        mode.label(),
        mode.description()
    ))
}

async fn compact(runtime: &Arc<Runtime>) -> CommandAction {
    match compact_session(runtime).await {
        Ok(message) => CommandAction::reply(message),
        Err(error) => CommandAction::reply(format!("Compaction failed: {error:#}")),
    }
}

async fn mcp(runtime: &Arc<Runtime>, args: &str) -> CommandAction {
    let configured = runtime.mcp.configured_names();
    if !runtime.mcp.has_servers() {
        return CommandAction::reply(
            "No MCP servers configured. Add `[mcp.<name>]` entries to ~/.aaykra/config.toml or .aaykra/config.toml.",
        );
    }
    match args.to_ascii_lowercase().as_str() {
        "" | "status" => CommandAction::reply(runtime.mcp.render_status().await),
        "discover" | "tools" | "resources" | "prompts" => {
            let discovery = runtime.mcp.discover(None).await;
            CommandAction::reply(crate::mcp::McpManager::render_discovery(&discovery))
        }
        _ => {
            let names: Vec<String> = args
                .split([',', ' '])
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned)
                .collect();
            let unknown: Vec<&str> = names
                .iter()
                .filter(|name| !configured.iter().any(|known| known == *name))
                .map(String::as_str)
                .collect();
            if !unknown.is_empty() {
                return CommandAction::reply(format!(
                    "Unknown MCP server(s): {}. Configured: {}",
                    unknown.join(", "),
                    configured.join(", ")
                ));
            }
            let discovery = runtime.mcp.discover(Some(&names)).await;
            CommandAction::reply(crate::mcp::McpManager::render_discovery(&discovery))
        }
    }
}

fn agent_catalog(runtime: &Arc<Runtime>, args: &str) -> CommandAction {
    if args.is_empty() {
        let mut out = runtime.agents.render_catalog();
        out.push_str("\nUse `/agents <name>` to switch, or the Agent tool for a one-off subagent.");
        return CommandAction::reply(out);
    }
    let Some(agent) = runtime.agents.get(args) else {
        return CommandAction::reply(format!(
            "Unknown agent `{args}`. Available: {}",
            runtime
                .agents
                .all()
                .iter()
                .map(|agent| agent.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    };
    let agent = agent.clone();
    let saved = {
        let Ok(mut session) = runtime.session.try_lock() else {
            return CommandAction::reply("Could not switch agents right now; try again.");
        };
        session.agent = agent.name.clone();
        session.updated_at = chrono::Utc::now();
        session.clone()
    };
    if let Err(error) = runtime.store.save(&saved) {
        return CommandAction::reply(format!("Switched agent, but could not save: {error:#}"));
    }
    CommandAction::reply(format!(
        "Agent set to `{}` — {}\nTools: {}\nMode: {}",
        agent.name,
        agent.description,
        agent
            .allowed_tools
            .as_ref()
            .filter(|tools| !tools.is_empty())
            .map(|tools| tools.join(", "))
            .unwrap_or_else(|| "all".into()),
        agent
            .permission_mode
            .map(|mode| mode.label().to_string())
            .unwrap_or_else(|| "inherit".into())
    ))
}

fn skills_catalog(runtime: &Arc<Runtime>, args: &str) -> CommandAction {
    if args.is_empty() {
        return CommandAction::reply(runtime.skills.render_catalog());
    }
    match runtime.skills.get(args) {
        Some(skill) => CommandAction::reply(format!(
            "/{}\n{}\nSource: {}\n\n{}",
            skill.name,
            skill.description,
            skill.source.label(),
            skill.body
        )),
        None => CommandAction::reply(format!(
            "Unknown skill `{args}`. Available: {}",
            runtime.skills.names().join(", ")
        )),
    }
}

fn memory(runtime: &Arc<Runtime>, args: &str) -> CommandAction {
    let store = MemoryStore::new(&runtime.project_root, &runtime.paths);
    if args.is_empty() {
        return CommandAction::reply(store.render());
    }
    let (action, text) = match args.split_once(char::is_whitespace) {
        Some((action, text)) => (action.to_ascii_lowercase(), text.trim()),
        None => (args.to_ascii_lowercase(), ""),
    };
    match action.as_str() {
        "add" if !text.is_empty() => match store.add(MemoryScope::Project, text) {
            Ok(()) => CommandAction::reply(format!(
                "Saved to {}.\n{}",
                store.project_path.display(),
                store.render()
            )),
            Err(error) => CommandAction::reply(format!("Could not save memory: {error:#}")),
        },
        "add-global" | "global" if !text.is_empty() => match store.add(MemoryScope::Global, text) {
            Ok(()) => CommandAction::reply(format!(
                "Saved to {}.\n{}",
                store.global_path.display(),
                store.render()
            )),
            Err(error) => CommandAction::reply(format!("Could not save memory: {error:#}")),
        },
        "remove" | "forget" if !text.is_empty() => match store.remove_matching(text) {
            Ok(removed) if removed.is_empty() => {
                CommandAction::reply(format!("No memory entries matched `{text}`."))
            }
            Ok(removed) => CommandAction::reply(format!(
                "Removed {} entr{}:\n{}",
                removed.len(),
                if removed.len() == 1 { "y" } else { "ies" },
                removed.join("\n")
            )),
            Err(error) => CommandAction::reply(format!("Could not edit memory: {error:#}")),
        },
        _ => CommandAction::reply(
            "Usage: `/memory`, `/memory add <note>`, `/memory add-global <note>`, `/memory remove <query>`",
        ),
    }
}

async fn diff(runtime: &Arc<Runtime>, args: &str) -> CommandAction {
    let cwd = runtime.cwd();
    let path = if args.is_empty() { None } else { Some(args) };
    let status = git_output(&cwd, &["status", "--short"]).await;
    let status = match status {
        Ok(status) => status,
        Err(error) => {
            return CommandAction::reply(format!(
                "Could not read git status in {}: {error:#}",
                cwd.display()
            ));
        }
    };
    let mut args_vec = vec!["diff", "--no-color", "--no-ext-diff"];
    if let Some(path) = path {
        args_vec.push("--");
        args_vec.push(path);
    }
    let unstaged = git_output(&cwd, &args_vec).await.unwrap_or_default();
    let mut staged_args = vec!["diff", "--cached", "--no-color", "--no-ext-diff"];
    if let Some(path) = path {
        staged_args.push("--");
        staged_args.push(path);
    }
    let staged = git_output(&cwd, &staged_args).await.unwrap_or_default();

    let mut out = String::new();
    out.push_str(if status.is_empty() {
        "Working tree clean (no tracked changes).\n"
    } else {
        "Git status:\n"
    });
    if !status.is_empty() {
        out.push_str(&status);
        out.push('\n');
    }
    if !unstaged.trim().is_empty() {
        out.push_str("\nUnstaged diff:\n");
        out.push_str(unstaged.trim_end());
        out.push('\n');
    }
    if !staged.trim().is_empty() {
        out.push_str("\nStaged diff:\n");
        out.push_str(staged.trim_end());
        out.push('\n');
    }
    if status.is_empty() && unstaged.trim().is_empty() && staged.trim().is_empty() {
        out.push_str("Tip: untracked files are shown in `git status --short`.");
    }
    CommandAction::reply(truncate_command_output(out))
}

async fn code_review(runtime: &Arc<Runtime>, args: &str) -> CommandAction {
    let cwd = runtime.cwd();
    let stat = match git_output(&cwd, &["diff", "--stat", "HEAD"]).await {
        Ok(stat) => stat,
        Err(_) => git_output(&cwd, &["diff", "--stat"])
            .await
            .unwrap_or_default(),
    };
    let status = git_output(&cwd, &["status", "--short"])
        .await
        .unwrap_or_default();
    let focus = if args.is_empty() {
        "Prioritize correctness bugs, security issues, regressions, and missing tests."
    } else {
        args
    };
    let prompt = format!(
        "Review the current working-tree changes in this repository. {focus}\n\n\
         Inspect the diff with the Git tool (status, diff, and show as needed) and read the relevant files for context. \
         Report findings ordered by severity with file:line references, explain the concrete failure mode, and suggest a minimal fix. \
         Call out missing tests and any risky permission or dependency changes. Do not modify files.\n\n\
         Git status:\n{status}\n\nDiff stat:\n{stat}"
    );
    CommandAction::RunPrompt(prompt)
}

fn init(_runtime: &Arc<Runtime>, args: &str) -> CommandAction {
    let extra = if args.is_empty() {
        String::new()
    } else {
        format!("\nAdditional guidance from the user: {args}")
    };
    CommandAction::RunPrompt(format!(
        "Initialize project instructions for Aaykra Code. Explore this repository (README, manifests, build/test commands, \
         directory layout, conventions, and existing CLAUDE.md/AGENTS.md/AAYKRA.md files). Then create or update `AAYKRA.md` \
         at the repository root with concise, actionable guidance: what the project is, how to build/lint/test, key directories, \
         architecture notes, coding conventions, and gotchas. Preserve useful existing content and do not invent commands you \
         have not verified. Keep it under roughly 150 lines.{extra}"
    ))
}

async fn undo(runtime: &Arc<Runtime>) -> CommandAction {
    match runtime.undo_last_turn().await {
        Ok(message) => CommandAction::reply(message),
        Err(error) => CommandAction::reply(format!("Undo failed: {error:#}")),
    }
}

fn permissions(runtime: &Arc<Runtime>) -> CommandAction {
    let config = &runtime.config;
    let session = runtime
        .session
        .try_lock()
        .map(|session| session.permission_mode)
        .unwrap_or_else(|_| runtime.permissions.mode());
    let mut out = format!(
        "Permission mode: {} ({})\nSandbox: {} · network: {}\n",
        session.label(),
        session.description(),
        config.sandbox.mode.label(),
        if config.sandbox.allow_network {
            "allowed"
        } else {
            "blocked"
        },
    );
    for (label, rules) in [
        ("allow", &config.permissions.allow),
        ("ask", &config.permissions.ask),
        ("deny", &config.permissions.deny),
    ] {
        out.push_str(&format!("\n{label}:\n"));
        if rules.is_empty() {
            out.push_str("  (none)\n");
        } else {
            for rule in rules {
                out.push_str(&format!("  {rule}\n"));
            }
        }
    }
    if !config.permissions.additional_directories.is_empty() {
        out.push_str("\nAdditional directories:\n");
        for directory in &config.permissions.additional_directories {
            out.push_str(&format!("  {}\n", directory.display()));
        }
    }
    out.push_str("\nRules use `Tool` or `Tool(pattern)`, e.g. `Bash(git push:*)`.");
    CommandAction::reply(out)
}

async fn status(runtime: &Arc<Runtime>) -> CommandAction {
    let session = runtime.session_snapshot().await;
    let context = session.estimated_context_tokens();
    let limit = runtime.context_limit();
    let percent = if limit == 0 {
        0.0
    } else {
        context as f64 / limit as f64 * 100.0
    };
    let mut out = String::new();
    out.push_str(&format!(
        "Session   {}  {}\n",
        &session.id[..8.min(session.id.len())],
        session.display_title()
    ));
    if session.name.is_some() {
        out.push_str(&format!("Name      {}\n", session.display_title()));
    }
    out.push_str(&format!(
        "Model     {}/{}\n",
        session.provider, session.model
    ));
    out.push_str(&format!(
        "Agent     {}\nMode      {} ({})\n",
        session.agent,
        session.permission_mode.label(),
        session.permission_mode.description()
    ));
    out.push_str(&format!("Directory {}\n", session.cwd.display()));
    if let Some(worktree) = &session.worktree {
        out.push_str(&format!("Worktree  {}\n", worktree.display()));
    }
    out.push_str(&format!(
        "Context   ~{} / {} tokens ({percent:.1}%)\n",
        compact_number(context as u64),
        limit
    ));
    out.push_str(&format!(
        "Usage     {}/{} tokens · {} · {} turns / {} messages\n",
        compact_number(session.usage.input_tokens),
        compact_number(session.usage.output_tokens),
        format_cost(session.cost_usd),
        session.turns.len(),
        session.messages.len()
    ));
    let todos_open = session
        .todos
        .iter()
        .filter(|todo| {
            matches!(
                todo.status,
                crate::types::TodoStatus::Pending | crate::types::TodoStatus::InProgress
            )
        })
        .count();
    out.push_str(&format!(
        "Todos     {todos_open} open / {} total\n",
        session.todos.len()
    ));
    out.push_str(&format!(
        "MCP       {} configured · LSP {} configured\n",
        runtime.mcp.configured_names().len(),
        runtime.config.lsp.len()
    ));
    out.push_str(&format!(
        "Updated   {}\n",
        session.updated_at.format("%Y-%m-%d %H:%M:%S UTC")
    ));
    CommandAction::reply(out)
}

async fn session_name(runtime: &Arc<Runtime>, args: &str) -> CommandAction {
    if args.is_empty() {
        let session = runtime.session_snapshot().await;
        return CommandAction::reply(format!(
            "Name: {}\nID: {}\nTitle: {}",
            session.name.as_deref().unwrap_or("(unnamed)"),
            session.id,
            session.title
        ));
    }
    let clear = matches!(
        args.to_ascii_lowercase().as_str(),
        "none" | "clear" | "-" | "unset"
    );
    {
        let mut session = runtime.session.lock().await;
        if let Err(error) = session.set_name(if clear { None } else { Some(args.to_string()) }) {
            return CommandAction::reply(format!("Could not set name: {error:#}"));
        }
    }
    if let Err(error) = runtime.save_session().await {
        return CommandAction::reply(format!("Name changed, but could not save: {error:#}"));
    }
    if clear {
        CommandAction::reply("Session name cleared.")
    } else {
        CommandAction::reply(format!("Session named `{args}`."))
    }
}

fn sessions(runtime: &Arc<Runtime>, args: &str) -> CommandAction {
    if args.is_empty() {
        return match runtime.store.list() {
            Ok(mut summaries) => {
                summaries.truncate(20);
                let mut out = SessionStore::render_list(&summaries);
                out.push_str("\nResume from the CLI with `akc --resume <id|name>`.");
                CommandAction::reply(out)
            }
            Err(error) => CommandAction::reply(format!("Could not list sessions: {error:#}")),
        };
    }
    // Switching the live session mid-TUI requires a transcript reload; keep the
    // command honest rather than half-applying it.
    CommandAction::reply(format!(
        "To resume `{args}`, start with `akc --resume {args}` (or `aaykra agent --resume {args}`). \
         Switching inside a running session is intentionally not supported yet."
    ))
}

async fn worktree(runtime: &Arc<Runtime>, args: &str) -> CommandAction {
    let session = runtime.session_snapshot().await;
    match args.to_ascii_lowercase().as_str() {
        "" | "status" => {
            if let Some(path) = session.worktree {
                CommandAction::reply(format!(
                    "Worktree: {}\nBranch: {}\nOriginal: {}",
                    path.display(),
                    session
                        .metadata
                        .get("worktree_branch")
                        .map(String::as_str)
                        .unwrap_or("(unknown)"),
                    session
                        .metadata
                        .get("original_cwd")
                        .map(String::as_str)
                        .unwrap_or("(unknown)")
                ))
            } else {
                CommandAction::reply(
                    "Not inside a worktree. Use `/worktree enter <name>` or the EnterWorktree tool.",
                )
            }
        }
        "exit" | "leave" => match runtime.exit_worktree(false, false).await {
            Ok(path) => CommandAction::reply(format!("Returned to {}.", path.display())),
            Err(error) => CommandAction::reply(format!("Could not exit worktree: {error:#}")),
        },
        other => {
            let name = other.strip_prefix("enter ").unwrap_or(other).trim();
            if name.is_empty() {
                return CommandAction::reply("Usage: `/worktree enter <name>` or `/worktree exit`");
            }
            match runtime.enter_worktree("", name).await {
                Ok(path) => CommandAction::reply(format!(
                    "Entered worktree at {}. The session working directory is now the worktree.",
                    path.display()
                )),
                Err(error) => CommandAction::reply(format!("Could not enter worktree: {error:#}")),
            }
        }
    }
}

fn truncate_command_output(mut text: String) -> String {
    const LIMIT: usize = 60_000;
    if text.len() > LIMIT {
        let mut boundary = LIMIT;
        while boundary > 0 && !text.is_char_boundary(boundary) {
            boundary -= 1;
        }
        text.truncate(boundary);
        text.push_str("\n… output truncated; pass a path to narrow `/diff`.");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_commands() {
        assert!(is_command("/help"));
        assert!(is_command("  /model list"));
        assert!(!is_command("hello /help"));
        assert!(!is_command("// literal slash"));
    }

    #[test]
    fn help_lists_core_commands() {
        for command in [
            "/model",
            "/plan",
            "/compact",
            "/context",
            "/mcp",
            "/agents",
            "/skills",
            "/memory",
            "/diff",
            "/code-review",
            "/init",
            "/undo",
            "/clear",
            "/permissions",
            "/status",
            "/name",
            "/sessions",
            "/quit",
        ] {
            assert!(help_text().contains(command), "missing {command}");
        }
    }
}
