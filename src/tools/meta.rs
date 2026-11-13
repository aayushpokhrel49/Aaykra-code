//! Agent-orchestration and session tools: todos, subagents, skills, questions,
//! plan/worktree transitions, checkpoints, memory, and deferred MCP.

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::agent;
use crate::memory::{MemoryScope, MemoryStore};
use crate::tools::{Tool, ToolContext, definition, permission_request, truncate_tool_output};
use crate::types::{
    PermissionMode, QuestionOption, RiskLevel, TodoItem, TodoStatus, ToolDefinition, UserQuestion,
};

pub struct TodoWriteTool;
pub struct AgentTool;
pub struct SkillTool;
pub struct AskUserQuestionTool;
pub struct EnterPlanModeTool;
pub struct ExitPlanModeTool;
pub struct EnterWorktreeTool;
pub struct ExitWorktreeTool;
pub struct CheckpointTool;
pub struct MemoryTool;
pub struct McpTool;

#[async_trait]
impl Tool for TodoWriteTool {
    fn definition(&self) -> ToolDefinition {
        definition(
            "TodoWrite",
            "Replace the session's todo list. Use this to plan multi-step work and keep progress visible. Mark exactly one item in_progress at a time; update statuses as you complete work.",
            json!({
                "type": "object",
                "properties": {
                    "todos": {
                        "type": "array",
                        "maxItems": 100,
                        "items": {
                            "type": "object",
                            "properties": {
                                "content": {"type": "string", "description": "Imperative task description."},
                                "status": {"type": "string", "enum": ["pending", "in_progress", "completed", "cancelled"]},
                                "active_form": {"type": "string", "description": "Present-continuous label shown while running."}
                            },
                            "required": ["content"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["todos"],
                "additionalProperties": false
            }),
        )
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn execute(&self, input: Value, context: &ToolContext) -> Result<String> {
        #[derive(Deserialize)]
        struct Item {
            content: String,
            #[serde(default)]
            status: TodoStatus,
            #[serde(default)]
            active_form: Option<String>,
        }
        let items: Vec<Item> =
            serde_json::from_value(input.get("todos").cloned().context("missing `todos`")?)
                .context("invalid todos array")?;
        let mut todos = Vec::with_capacity(items.len());
        for item in items {
            let content = item.content.trim();
            if content.is_empty() {
                bail!("todo content must not be empty");
            }
            todos.push(TodoItem {
                content: content.to_string(),
                status: item.status,
                active_form: item.active_form.filter(|value| !value.trim().is_empty()),
            });
        }
        let open = todos
            .iter()
            .filter(|todo| matches!(todo.status, TodoStatus::Pending | TodoStatus::InProgress))
            .count();
        let done = todos
            .iter()
            .filter(|todo| todo.status == TodoStatus::Completed)
            .count();
        context.update_todos(todos.clone()).await;
        let mut out = format!("Updated todos: {open} open, {done} completed.\n");
        for todo in &todos {
            out.push_str(&format!(
                "{} {}{}\n",
                todo.status.symbol(),
                todo.content,
                if todo.status == TodoStatus::InProgress {
                    " (in progress)"
                } else {
                    ""
                }
            ));
        }
        Ok(out)
    }
}

#[async_trait]
impl Tool for AgentTool {
    fn definition(&self) -> ToolDefinition {
        definition(
            "Agent",
            "Delegate a focused task to a specialized subagent running in an isolated session. Use `explore` for broad read-only codebase investigation, `plan` for architecture, `reviewer` for review, or a custom configured agent. The subagent returns a concise report; it cannot ask you follow-up questions. Do not delegate work you can finish directly with one or two tools.",
            json!({
                "type": "object",
                "properties": {
                    "description": {"type": "string", "description": "Short 3-6 word label for the UI."},
                    "prompt": {"type": "string", "description": "Complete, self-contained task with the question, scope, and expected output."},
                    "agent": {"type": "string", "description": "Agent name. Defaults to `explore`."}
                },
                "required": ["description", "prompt"],
                "additionalProperties": false
            }),
        )
    }

    async fn execute(&self, input: Value, context: &ToolContext) -> Result<String> {
        context.ensure_not_cancelled()?;
        let prompt = input
            .get("prompt")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .context("missing `prompt`")?;
        let description = input
            .get("description")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("subagent task");
        let name = input
            .get("agent")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("explore");
        if context.runtime.agents.get(name).is_none() {
            let available = context
                .runtime
                .agents
                .all()
                .iter()
                .map(|agent| agent.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            bail!("unknown agent `{name}`; available agents: {available}");
        }
        let outcome = agent::run_subagent(&context.runtime, name, prompt, description).await?;
        Ok(truncate_tool_output(
            context,
            format!(
                "Agent `{name}` finished ({}, {} tokens in / {} out, {}).\n\n{}",
                outcome.stop_reason,
                outcome.usage.input_tokens,
                outcome.usage.output_tokens,
                crate::models::format_cost(outcome.cost_usd),
                outcome.text.trim()
            ),
        ))
    }
}

#[async_trait]
impl Tool for SkillTool {
    fn definition(&self) -> ToolDefinition {
        definition(
            "Skill",
            "Load a skill's workflow instructions and follow them for the current task. Skills are reusable procedures (code review, commit, debugging, security review) plus project-specific workflows.",
            json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "Skill name without the leading slash."},
                    "arguments": {"type": "string", "description": "Optional arguments or focus for the skill."}
                },
                "required": ["name"],
                "additionalProperties": false
            }),
        )
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn execute(&self, input: Value, context: &ToolContext) -> Result<String> {
        let name = input
            .get("name")
            .and_then(Value::as_str)
            .map(|value| value.trim().trim_start_matches('/'))
            .filter(|value| !value.is_empty())
            .context("missing `name`")?;
        let Some(skill) = context.runtime.skills.get(name) else {
            bail!(
                "unknown skill `{name}`; available: {}",
                context.runtime.skills.names().join(", ")
            );
        };
        let mut out = skill.prompt();
        if let Some(arguments) = input
            .get("arguments")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
        {
            out.push_str(&format!("\n\nArguments: {arguments}"));
        }
        Ok(truncate_tool_output(context, out))
    }
}

#[async_trait]
impl Tool for AskUserQuestionTool {
    fn definition(&self) -> ToolDefinition {
        definition(
            "AskUserQuestion",
            "Ask the user a focused multiple-choice question when a decision blocks progress. Provide 2-4 concrete options. Use sparingly; prefer deciding from the repository when possible.",
            json!({
                "type": "object",
                "properties": {
                    "question": {"type": "string"},
                    "header": {"type": "string", "description": "Short label (max 12 chars)."},
                    "options": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": 4,
                        "items": {
                            "type": "object",
                            "properties": {
                                "label": {"type": "string"},
                                "description": {"type": "string"}
                            },
                            "required": ["label"],
                            "additionalProperties": false
                        }
                    },
                    "multiple": {"type": "boolean", "default": false}
                },
                "required": ["question", "options"],
                "additionalProperties": false
            }),
        )
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn execute(&self, input: Value, context: &ToolContext) -> Result<String> {
        let question = input
            .get("question")
            .and_then(Value::as_str)
            .context("missing `question`")?
            .to_string();
        let header = input
            .get("header")
            .and_then(Value::as_str)
            .unwrap_or("Question")
            .to_string();
        let options: Vec<QuestionOption> =
            serde_json::from_value(input.get("options").cloned().context("missing `options`")?)
                .context("invalid options array")?;
        let answer = context
            .runtime
            .ui
            .ask(UserQuestion {
                question,
                header,
                options,
                multiple: input
                    .get("multiple")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            })
            .await;
        Ok(match answer {
            Some(answer) => format!("User answered: {}", answer.as_text()),
            None => "The user did not answer the question.".into(),
        })
    }
}

#[async_trait]
impl Tool for EnterPlanModeTool {
    fn definition(&self) -> ToolDefinition {
        definition(
            "EnterPlanMode",
            "Switch to read-only plan mode. Do this before proposing a substantial implementation so the user can review the approach without any files changing.",
            json!({"type": "object", "properties": {}, "additionalProperties": false}),
        )
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn execute(&self, _input: Value, context: &ToolContext) -> Result<String> {
        let mode = context.runtime.enter_plan_mode();
        Ok(format!(
            "Plan mode enabled ({}). Investigate read-only, then present a concrete plan and call ExitPlanMode for approval.",
            mode.label()
        ))
    }
}

#[async_trait]
impl Tool for ExitPlanModeTool {
    fn definition(&self) -> ToolDefinition {
        definition(
            "ExitPlanMode",
            "Present the finished implementation plan to the user and request approval to leave read-only plan mode. Include concrete files, changes, ordering, tests, and risks.",
            json!({
                "type": "object",
                "properties": {
                    "plan": {"type": "string", "description": "The complete implementation plan in Markdown."}
                },
                "required": ["plan"],
                "additionalProperties": false
            }),
        )
    }

    async fn execute(&self, input: Value, context: &ToolContext) -> Result<String> {
        let plan = input
            .get("plan")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .context("missing `plan`")?;
        if context.runtime.permissions.mode() != PermissionMode::Plan {
            return Ok("Not in plan mode; continue with the approved work.".into());
        }
        let mut request = permission_request(
            "ExitPlanMode",
            "approve the implementation plan",
            "approve the plan and allow edits",
            false,
            RiskLevel::Medium,
            Vec::new(),
        );
        request.diff = Some(plan.to_string());
        request.suggested_rule = "ExitPlanMode".into();
        let decision = context.runtime.ui.authorize(request).await;
        match decision {
            crate::types::PermissionDecision::Allow
            | crate::types::PermissionDecision::AllowForSession { .. } => {
                let mode = context.runtime.exit_plan_mode();
                Ok(format!(
                    "Plan approved. Mode is now {}. Implement the plan and verify the result.",
                    mode.label()
                ))
            }
            crate::types::PermissionDecision::Deny { reason } => bail!(
                "plan rejected{}; stay in plan mode and revise it",
                reason
                    .map(|reason| format!(": {reason}"))
                    .unwrap_or_default()
            ),
        }
    }
}

#[async_trait]
impl Tool for EnterWorktreeTool {
    fn definition(&self) -> ToolDefinition {
        definition(
            "EnterWorktree",
            "Create and switch into an isolated git worktree under `.aaykra/worktrees/<name>` on a new or existing branch. All subsequent tools operate there. Use this for risky or parallel work.",
            json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "Short worktree name."},
                    "branch": {"type": "string", "description": "Branch to create/use. Defaults to aaykra/<name>."}
                },
                "required": ["name"],
                "additionalProperties": false
            }),
        )
    }

    async fn execute(&self, input: Value, context: &ToolContext) -> Result<String> {
        let name = input
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .context("missing `name`")?;
        let branch = input
            .get("branch")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let mut request = permission_request(
            "EnterWorktree",
            name.to_string(),
            "create and enter an isolated git worktree",
            false,
            RiskLevel::Medium,
            vec![context.cwd()],
        );
        request.suggested_rule = "EnterWorktree".into();
        context.authorize(request).await?;
        let path = context.runtime.enter_worktree(branch, name).await?;
        Ok(format!(
            "Entered worktree at {}. The session now operates there.",
            path.display()
        ))
    }
}

#[async_trait]
impl Tool for ExitWorktreeTool {
    fn definition(&self) -> ToolDefinition {
        definition(
            "ExitWorktree",
            "Leave the current Aaykra worktree and return to the original project. Pass force=true to leave with uncommitted changes, and remove=true to delete the worktree directory.",
            json!({
                "type": "object",
                "properties": {
                    "force": {"type": "boolean", "default": false},
                    "remove": {"type": "boolean", "default": false}
                },
                "additionalProperties": false
            }),
        )
    }

    async fn execute(&self, input: Value, context: &ToolContext) -> Result<String> {
        let force = input.get("force").and_then(Value::as_bool).unwrap_or(false);
        let remove = input
            .get("remove")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let mut request = permission_request(
            "ExitWorktree",
            "leave the worktree",
            if force {
                "leave the worktree, discarding uncommitted files"
            } else {
                "leave the worktree"
            },
            false,
            if force {
                RiskLevel::High
            } else {
                RiskLevel::Medium
            },
            vec![context.cwd()],
        );
        request.suggested_rule = "ExitWorktree".into();
        context.authorize(request).await?;
        let path = context.runtime.exit_worktree(force, remove).await?;
        Ok(format!(
            "Exited the worktree and returned to {}.{}",
            path.display(),
            if remove {
                " The worktree directory was removed."
            } else {
                " The worktree directory was kept."
            }
        ))
    }
}

#[async_trait]
impl Tool for CheckpointTool {
    fn definition(&self) -> ToolDefinition {
        definition(
            "Checkpoint",
            "List checkpointed turns or restore files from a checkpoint. `undo` restores the last turn's files and removes its messages; `restore` restores files for a specific turn without changing the transcript.",
            json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["list", "restore", "undo"]},
                    "turn": {"type": "integer", "minimum": 1, "description": "Turn number for restore. Defaults to the latest checkpoint."}
                },
                "required": ["action"],
                "additionalProperties": false
            }),
        )
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn execute(&self, input: Value, context: &ToolContext) -> Result<String> {
        let action = input
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("list");
        let session_id = context.runtime.session_id();
        match action {
            "list" => {
                let turns = context.runtime.checkpointer.list_turns(&session_id);
                if turns.is_empty() {
                    Ok("No checkpoints for this session yet.".into())
                } else {
                    Ok(format!(
                        "Checkpointed turns: {}",
                        turns
                            .iter()
                            .map(ToString::to_string)
                            .collect::<Vec<_>>()
                            .join(", ")
                    ))
                }
            }
            "restore" => {
                let turns = context.runtime.checkpointer.list_turns(&session_id);
                let turn = input
                    .get("turn")
                    .and_then(Value::as_u64)
                    .or_else(|| turns.last().copied())
                    .context("no checkpointed turn to restore")?;
                let mut request = permission_request(
                    "Checkpoint",
                    format!("restore turn {turn}"),
                    "restore files from a checkpoint",
                    false,
                    RiskLevel::High,
                    Vec::new(),
                );
                request.suggested_rule = "Checkpoint".into();
                context.authorize(request).await?;
                let report = context
                    .runtime
                    .checkpointer
                    .restore_turn(&session_id, turn)?;
                Ok(format!("Restored turn {turn}.\n{}", report.render()))
            }
            "undo" => {
                let mut request = permission_request(
                    "Checkpoint",
                    "undo the last turn",
                    "restore files and remove the last turn",
                    false,
                    RiskLevel::High,
                    Vec::new(),
                );
                request.suggested_rule = "Checkpoint".into();
                context.authorize(request).await?;
                context.runtime.undo_last_turn().await
            }
            other => bail!("unknown checkpoint action `{other}` (list, restore, undo)"),
        }
    }
}

#[async_trait]
impl Tool for MemoryTool {
    fn definition(&self) -> ToolDefinition {
        definition(
            "Memory",
            "Maintain durable project knowledge in `.aaykra/MEMORY.md` (project scope) or `~/.aaykra/MEMORY.md` (global scope). Add concise, non-secret notes that will matter in future sessions. Never store credentials.",
            json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["list", "add", "remove"]},
                    "scope": {"type": "string", "enum": ["project", "global"], "description": "Defaults to project."},
                    "note": {"type": "string", "description": "Memory to add (add)."},
                    "query": {"type": "string", "description": "Substring of memories to remove (remove)."}
                },
                "required": ["action"],
                "additionalProperties": false
            }),
        )
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn execute(&self, input: Value, context: &ToolContext) -> Result<String> {
        let action = input
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("list");
        let store = MemoryStore::new(&context.runtime.project_root, &context.runtime.paths);
        let scope = match input
            .get("scope")
            .and_then(Value::as_str)
            .unwrap_or("project")
        {
            "global" => MemoryScope::Global,
            "project" => MemoryScope::Project,
            other => bail!("unknown memory scope `{other}` (project, global)"),
        };
        match action {
            "list" => Ok(store.render()),
            "add" => {
                let note = input
                    .get("note")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .context("missing `note`")?;
                let mut request = permission_request(
                    "Memory",
                    format!("add a {} memory", scope.label()),
                    "save durable project knowledge",
                    false,
                    RiskLevel::Medium,
                    vec![store.path_for(scope).to_path_buf()],
                );
                request.suggested_rule = "Memory".into();
                context.authorize(request).await?;
                store.add(scope, note)?;
                Ok(format!("Saved to {} memory.", scope.label()))
            }
            "remove" => {
                let query = input
                    .get("query")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .context("missing `query`")?;
                let mut request = permission_request(
                    "Memory",
                    format!("remove memories matching `{query}`"),
                    "remove durable project knowledge",
                    false,
                    RiskLevel::Medium,
                    vec![store.global_path.clone(), store.project_path.clone()],
                );
                request.suggested_rule = "Memory".into();
                context.authorize(request).await?;
                let removed = store.remove_matching(query)?;
                if removed.is_empty() {
                    Ok(format!("No memories matched `{query}`."))
                } else {
                    Ok(format!(
                        "Removed {} memories:\n{}",
                        removed.len(),
                        removed.join("\n")
                    ))
                }
            }
            other => bail!("unknown memory action `{other}` (list, add, remove)"),
        }
    }
}

#[async_trait]
impl Tool for McpTool {
    fn definition(&self) -> ToolDefinition {
        definition(
            "Mcp",
            "Use configured Model Context Protocol servers. `list` discovers tools, resources, and prompts without loading every schema into context. Then use `call` for a tool, `read_resource` for a resource URI, or `get_prompt` for a prompt.",
            json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["list", "call", "read_resource", "get_prompt"]},
                    "server": {"type": "string", "description": "MCP server name."},
                    "tool": {"type": "string", "description": "Tool name (call)."},
                    "arguments": {"type": "object", "description": "Tool or prompt arguments."},
                    "uri": {"type": "string", "description": "Resource URI (read_resource)."},
                    "prompt": {"type": "string", "description": "Prompt name (get_prompt)."}
                },
                "required": ["action"],
                "additionalProperties": false
            }),
        )
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn execute(&self, input: Value, context: &ToolContext) -> Result<String> {
        let action = input
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("list");
        let mut request = permission_request(
            "Mcp",
            action.to_string(),
            if action == "call" {
                "call an MCP tool"
            } else {
                "read from an MCP server"
            },
            action != "call",
            if action == "call" {
                RiskLevel::Medium
            } else {
                RiskLevel::Low
            },
            Vec::new(),
        );
        request.suggested_rule = format!("Mcp({action}:*)");
        context.authorize(request).await?;

        match action {
            "list" => {
                if !context.runtime.mcp.has_servers() {
                    return Ok("No MCP servers are configured.".into());
                }
                let only = context.mcp_allowed.as_deref();
                if only.is_some_and(<[String]>::is_empty) {
                    return Ok("MCP is disabled for this agent (no servers are allowed).".into());
                }
                let discovery = context.runtime.mcp.discover(only).await;
                Ok(truncate_tool_output(
                    context,
                    crate::mcp::McpManager::render_discovery(&discovery),
                ))
            }
            "call" => {
                let server = require_mcp_server(&input, context)?;
                let tool = input
                    .get("tool")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .context("missing `tool`")?;
                let arguments = input.get("arguments").cloned().unwrap_or_else(|| json!({}));
                let output = context
                    .runtime
                    .mcp
                    .call_tool(server, tool, arguments)
                    .await?;
                Ok(truncate_tool_output(
                    context,
                    format!("[{server}/{tool}]\n{output}"),
                ))
            }
            "read_resource" => {
                let server = require_mcp_server(&input, context)?;
                let uri = input
                    .get("uri")
                    .and_then(Value::as_str)
                    .context("missing `uri`")?;
                let output = context.runtime.mcp.read_resource(server, uri).await?;
                Ok(truncate_tool_output(
                    context,
                    format!("[{server}] {uri}\n{output}"),
                ))
            }
            "get_prompt" => {
                let server = require_mcp_server(&input, context)?;
                let prompt = input
                    .get("prompt")
                    .and_then(Value::as_str)
                    .context("missing `prompt`")?;
                let arguments = input.get("arguments").cloned().unwrap_or_else(|| json!({}));
                let output = context
                    .runtime
                    .mcp
                    .get_prompt(server, prompt, arguments)
                    .await?;
                Ok(truncate_tool_output(
                    context,
                    format!("[{server}/{prompt}]\n{output}"),
                ))
            }
            other => bail!("unknown MCP action `{other}` (list, call, read_resource, get_prompt)"),
        }
    }
}

fn require_mcp_server<'a>(input: &'a Value, context: &ToolContext) -> Result<&'a str> {
    let server = input
        .get("server")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .context("missing `server`")?;
    if let Some(allowed) = &context.mcp_allowed
        && !allowed.iter().any(|name| name == server)
    {
        bail!(
            "MCP server `{server}` is not allowed for agent `{}`; allowed: {}",
            context.agent_name,
            if allowed.is_empty() {
                "none".to_string()
            } else {
                allowed.join(", ")
            }
        );
    }
    Ok(server)
}
