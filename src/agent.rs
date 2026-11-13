//! The agent loop: streaming, tool dispatch, permissions, checkpoints,
//! compaction, hooks, and subagents.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::agents::AgentDefinition;
use crate::config::Effort;
use crate::hooks::HookKind;
use crate::memory::{self, MemoryScope, MemoryStore};
use crate::models::{estimate_cost, resolve_model};
use crate::providers::{ChatRequest, Provider, StopReason, StreamEvent, resolve_fallbacks};
use crate::runtime::Runtime;
use crate::session::Session;
use crate::tools::{ToolContext, ToolRegistry, truncate_tool_output};
use crate::types::{
    AgentEvent, ContentBlock, Message, PermissionDecision, PermissionRequest, Role, TodoItem,
    UiGateway, Usage, UserAnswer, UserQuestion,
};

const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(300);
const SUMMARY_KEEP_MESSAGES: usize = 8;
const HANDOFF_KEEP_TURNS: usize = 2;
const HANDOFF_MAX_CHARS: usize = 12_000;
const REPO_MAP_MAX_CHARS: usize = 6_000;

#[derive(Debug, Clone, Default)]
pub struct RunOutcome {
    pub text: String,
    pub stop_reason: String,
    pub usage: Usage,
    pub cost_usd: f64,
}

#[derive(Debug)]
struct ModelCall {
    provider: String,
    model: String,
    text: String,
    thinking: String,
    signature: Option<String>,
    tool_calls: Vec<ToolCall>,
    usage: Usage,
    stop_reason: StopReason,
    cancelled: bool,
}

#[derive(Debug, Clone)]
struct ToolCall {
    id: String,
    name: String,
    input: Value,
    input_error: Option<String>,
}

/// Distinguishes a failed request (safe to retry with a fallback) from a stream
/// that failed after content was already shown (not safe to replay).
#[derive(Debug)]
struct StreamFailure {
    started: bool,
    error: anyhow::Error,
}

#[derive(Debug, Default)]
struct PendingTool {
    id: String,
    name: String,
    arguments: String,
}

/// Execute one user turn against the session. Messages already in the session are
/// the conversation history, so this naturally supports `-c` and resume.
pub async fn run_turn(
    runtime: Arc<Runtime>,
    message: Message,
    max_turns: Option<u32>,
) -> Result<RunOutcome> {
    let prompt_text = message.text();
    let mut hook = runtime.hook_context(HookKind::UserPromptSubmit).await;
    hook.prompt = Some(prompt_text.clone());
    let outcome = runtime.run_hook(HookKind::UserPromptSubmit, &hook).await;
    emit_hook_messages(&runtime, &outcome);
    if outcome.blocked {
        bail!(
            "prompt blocked by UserPromptSubmit hook{}",
            outcome
                .reason
                .map(|reason| format!(": {reason}"))
                .unwrap_or_default()
        );
    }

    if runtime.config.auto_memory
        && let Some(note) = memory::auto_memory_candidate(&prompt_text)
    {
        let store = MemoryStore::new(&runtime.project_root, &runtime.paths);
        match store.add(MemoryScope::Project, &note) {
            Ok(()) => runtime.emit(AgentEvent::Notice(format!(
                "Remembered: {}",
                note.chars().take(120).collect::<String>()
            ))),
            Err(error) => runtime.emit(AgentEvent::Warning(format!(
                "could not save automatic memory: {error:#}"
            ))),
        }
    }

    let handoff = match prepare_context_handoff(&runtime).await {
        Ok(handoff) => handoff,
        Err(error) => {
            runtime.emit(AgentEvent::Warning(format!(
                "could not prepare the context handoff: {error:#}"
            )));
            false
        }
    };
    let (first_turn, agent_name) = {
        let session = runtime.session.lock().await;
        (session.messages.is_empty(), session.agent.clone())
    };
    let agent = runtime
        .agents
        .get(&agent_name)
        .cloned()
        .unwrap_or_else(|| runtime.agents.default_agent().clone());
    let registry = tool_registry(&runtime, &agent);
    let tool_context = ToolContext {
        runtime: runtime.clone(),
        agent_name: agent.name.clone(),
        mcp_allowed: agent.mcp_servers.clone(),
    };

    let turn = {
        let mut session = runtime.session.lock().await;
        session.begin_turn(message)
    };
    runtime.emit(AgentEvent::TurnStarted { turn });
    runtime.save_session().await?;

    let system = build_system_prompt(&runtime, &agent, first_turn || handoff).await;
    let limit = max_turns
        .or(agent.max_turns)
        .unwrap_or(runtime.config.max_turns)
        .max(1);
    let mut usage = Usage::default();
    let mut cost = 0.0;
    let mut last_text = String::new();
    let result = run_loop(
        &runtime,
        &system,
        &registry,
        &tool_context,
        limit,
        &mut usage,
        &mut cost,
        &mut last_text,
    )
    .await;

    let stop_reason = match &result {
        Ok(reason) => reason.clone(),
        Err(error) => {
            let reason = if runtime.cancel.is_cancelled() {
                "cancelled".to_string()
            } else {
                "error".to_string()
            };
            finish_turn(&runtime, usage, cost, &reason).await;
            let message = format!("agent run failed: {error:#}");
            runtime.emit(AgentEvent::RunFailed(message.clone()));
            return Err(anyhow::anyhow!(message));
        }
    };
    finish_turn(&runtime, usage, cost, &stop_reason).await;

    let mut stop_hook = runtime.hook_context(HookKind::Stop).await;
    stop_hook.prompt = Some(last_text.clone());
    let outcome = runtime.run_hook(HookKind::Stop, &stop_hook).await;
    emit_hook_messages(&runtime, &outcome);

    Ok(RunOutcome {
        text: last_text,
        stop_reason,
        usage,
        cost_usd: cost,
    })
}

async fn finish_turn(runtime: &Arc<Runtime>, usage: Usage, cost: f64, stop_reason: &str) {
    let (turn, session_cost) = {
        let mut session = runtime.session.lock().await;
        session.finish_turn(usage, cost, stop_reason);
        if runtime.config.sessions.auto_save {
            let _ = runtime.store.save(&session);
        }
        (session.current_turn, session.cost_usd)
    };
    runtime.emit(AgentEvent::TurnFinished {
        turn,
        stop_reason: stop_reason.to_string(),
        usage,
        cost_usd: cost,
        session_cost_usd: session_cost,
    });
}

async fn run_loop(
    runtime: &Arc<Runtime>,
    system: &str,
    registry: &ToolRegistry,
    tool_context: &ToolContext,
    max_turns: u32,
    usage: &mut Usage,
    cost: &mut f64,
    last_text: &mut String,
) -> Result<String> {
    let definitions = registry.definitions();
    let mut iterations = 0u32;
    loop {
        if runtime.cancel.is_cancelled() {
            return Ok("cancelled".into());
        }
        if iterations >= max_turns {
            runtime.emit(AgentEvent::Warning(format!(
                "stopped after {max_turns} model turns; send another message to continue"
            )));
            return Ok("max_turns".into());
        }

        if let Some((compact_usage, compact_cost)) = maybe_compact(runtime).await? {
            usage.add(compact_usage);
            *cost += compact_cost;
        }
        iterations += 1;

        let messages = {
            let session = runtime.session.lock().await;
            session.messages.clone()
        };
        if messages.is_empty() {
            bail!("session has no messages");
        }
        let mut call =
            call_model(runtime, system, &messages, &definitions, &mut *usage, cost).await?;

        if call.cancelled {
            let assistant = assistant_message(&call);
            if let Some(assistant) = assistant {
                let mut session = runtime.session.lock().await;
                session.push_message(assistant);
                runtime.store.save(&session)?;
            }
            return Ok("cancelled".into());
        }
        if !call.text.trim().is_empty() {
            *last_text = call.text.clone();
        }
        runtime.emit(AgentEvent::Usage {
            turn: *usage,
            session_usage: session_usage_with(runtime, usage).await,
            session_cost_usd: session_cost_with(runtime, *cost).await,
        });

        let assistant = assistant_message(&call);
        let has_tools = !call.tool_calls.is_empty();
        if assistant.is_none() && !has_tools {
            runtime.emit(AgentEvent::Warning(
                "the model returned an empty response".into(),
            ));
            return Ok(call.stop_reason.label());
        }
        if let Some(assistant) = assistant {
            let mut session = runtime.session.lock().await;
            session.push_message(assistant);
            if runtime.config.sessions.auto_save {
                runtime.store.save(&session)?;
            }
        }
        if !has_tools {
            return Ok(call.stop_reason.label());
        }

        let mut results = Vec::new();
        for tool_call in std::mem::take(&mut call.tool_calls) {
            let result = execute_tool(runtime, registry, tool_context, tool_call).await;
            results.push(result);
            if runtime.cancel.is_cancelled() {
                break;
            }
        }
        let mut session = runtime.session.lock().await;
        session.push_message(Message {
            role: Role::User,
            content: results,
        });
        if runtime.config.sessions.auto_save {
            runtime.store.save(&session)?;
        }
        drop(session);
        if runtime.cancel.is_cancelled() {
            return Ok("cancelled".into());
        }
    }
}

async fn execute_tool(
    runtime: &Arc<Runtime>,
    registry: &ToolRegistry,
    tool_context: &ToolContext,
    tool_call: ToolCall,
) -> ContentBlock {
    let display_input = serde_json::to_string_pretty(&tool_call.input)
        .unwrap_or_else(|_| tool_call.input.to_string());
    runtime.emit(AgentEvent::ToolStarted {
        id: tool_call.id.clone(),
        name: tool_call.name.clone(),
        input: truncate(&display_input, 8_000),
    });

    let mut hook = runtime.hook_context(HookKind::PreToolUse).await;
    hook.tool_name = Some(tool_call.name.clone());
    hook.tool_input = Some(tool_call.input.clone());
    let outcome = runtime.run_hook(HookKind::PreToolUse, &hook).await;
    emit_hook_messages(runtime, &outcome);
    if outcome.blocked {
        let message = format!(
            "blocked by PreToolUse hook{}",
            outcome
                .reason
                .map(|reason| format!(": {reason}"))
                .unwrap_or_default()
        );
        runtime.emit(AgentEvent::ToolFinished {
            id: tool_call.id.clone(),
            name: tool_call.name.clone(),
            output: message.clone(),
            is_error: true,
        });
        return tool_result(&tool_call.id, message, true);
    }

    let Some(tool) = registry.get(&tool_call.name) else {
        let message = format!(
            "unknown or disabled tool `{}`; available tools: {}",
            tool_call.name,
            registry.names().join(", ")
        );
        runtime.emit(AgentEvent::ToolFinished {
            id: tool_call.id.clone(),
            name: tool_call.name.clone(),
            output: message.clone(),
            is_error: true,
        });
        return tool_result(&tool_call.id, message, true);
    };

    if let Some(error) = &tool_call.input_error {
        let message = format!("invalid tool input JSON: {error}");
        runtime.emit(AgentEvent::ToolFinished {
            id: tool_call.id.clone(),
            name: tool_call.name.clone(),
            output: message.clone(),
            is_error: true,
        });
        return tool_result(&tool_call.id, message, true);
    }

    let result = tool.execute(tool_call.input.clone(), tool_context).await;
    let (mut output, is_error) = match result {
        Ok(output) => (output, false),
        Err(error) => (format!("Error: {error:#}"), true),
    };
    if output.trim().is_empty() {
        output = "(no output)".into();
    }
    output = truncate_tool_output(tool_context, output);

    let mut post = runtime.hook_context(HookKind::PostToolUse).await;
    post.tool_name = Some(tool_call.name.clone());
    post.tool_input = Some(tool_call.input.clone());
    post.tool_output = Some(output.clone());
    post.tool_error = is_error;
    let outcome = runtime.run_hook(HookKind::PostToolUse, &post).await;
    emit_hook_messages(runtime, &outcome);
    if outcome.blocked && !is_error {
        output.push_str(&format!(
            "\n\n[PostToolUse hook warning{}]",
            outcome
                .reason
                .map(|reason| format!(": {reason}"))
                .unwrap_or_default()
        ));
    }

    runtime.emit(AgentEvent::ToolFinished {
        id: tool_call.id.clone(),
        name: tool_call.name.clone(),
        output: truncate(&output, 12_000),
        is_error,
    });
    tool_result(&tool_call.id, output, is_error)
}

fn tool_result(id: &str, content: String, is_error: bool) -> ContentBlock {
    ContentBlock::ToolResult {
        tool_use_id: id.to_string(),
        content,
        is_error,
    }
}

fn assistant_message(call: &ModelCall) -> Option<Message> {
    let mut content = Vec::new();
    if !call.thinking.is_empty() {
        content.push(ContentBlock::Thinking {
            thinking: call.thinking.clone(),
            signature: call.signature.clone(),
        });
    }
    if !call.text.is_empty() {
        content.push(ContentBlock::Text {
            text: call.text.clone(),
        });
    }
    for tool in &call.tool_calls {
        content.push(ContentBlock::ToolUse {
            id: tool.id.clone(),
            name: tool.name.clone(),
            input: tool.input.clone(),
        });
    }
    if content.is_empty() {
        None
    } else {
        Some(Message {
            role: Role::Assistant,
            content,
        })
    }
}

async fn call_model(
    runtime: &Arc<Runtime>,
    system: &str,
    messages: &[Message],
    definitions: &[crate::types::ToolDefinition],
    usage: &mut Usage,
    cost: &mut f64,
) -> Result<ModelCall> {
    let session = runtime.session_snapshot().await;
    let primary = resolve_model(
        &runtime.config,
        Some(&session.model),
        Some(&session.provider),
    )?;
    let mut candidates = vec![primary.reference.clone()];
    candidates.extend(resolve_fallbacks(&runtime.config, &primary.reference));
    let base = ChatRequest {
        model: String::new(),
        system: system.to_string(),
        messages: messages.to_vec(),
        tools: definitions.to_vec(),
        max_tokens: runtime.config.max_tokens,
        temperature: runtime.config.temperature,
        effort: runtime.config.effort,
        stream: true,
    };
    let mut last_error: Option<anyhow::Error> = None;
    for (index, candidate) in candidates.iter().enumerate() {
        let resolved = match resolve_model(
            &runtime.config,
            Some(&candidate.model),
            Some(&candidate.provider),
        ) {
            Ok(resolved) => resolved,
            Err(error) => {
                last_error = Some(error);
                continue;
            }
        };
        let provider = match runtime.provider_runtime.build(&resolved) {
            Ok(provider) => provider,
            Err(error) => {
                last_error = Some(error);
                continue;
            }
        };
        let mut request = base.clone();
        request.model = resolved.reference.model.clone();
        match stream_call(runtime, provider.as_ref(), request).await {
            Ok(call) => {
                if index > 0 || call.provider != session.provider || call.model != session.model {
                    let mut state = runtime.session.lock().await;
                    state.provider = call.provider.clone();
                    state.model = call.model.clone();
                    if runtime.config.sessions.auto_save {
                        let _ = runtime.store.save(&state);
                    }
                    runtime.emit(AgentEvent::ModelChanged {
                        provider: call.provider.clone(),
                        model: call.model.clone(),
                    });
                }
                usage.add(call.usage);
                *cost += estimate_cost(&runtime.config, &call.provider, &call.model, &call.usage);
                return Ok(call);
            }
            Err(failure) if failure.started => return Err(failure.error),
            Err(failure) => {
                last_error = Some(failure.error);
                if index + 1 < candidates.len() {
                    runtime.emit(AgentEvent::Warning(format!(
                        "model {}/{} failed: {:#}; trying the next fallback",
                        resolved.reference.provider,
                        resolved.reference.model,
                        last_error.as_ref().unwrap()
                    )));
                }
            }
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("no model is configured")))
}

async fn stream_call(
    runtime: &Arc<Runtime>,
    provider: &dyn Provider,
    request: ChatRequest,
) -> Result<ModelCall, StreamFailure> {
    let mut stream = provider
        .stream_chat(request)
        .await
        .map_err(|error| StreamFailure {
            started: false,
            error,
        })?;
    let mut model_call = ModelCall {
        provider: provider.provider_name().to_string(),
        model: provider.model_name().to_string(),
        text: String::new(),
        thinking: String::new(),
        signature: None,
        tool_calls: Vec::new(),
        usage: Usage::default(),
        stop_reason: StopReason::EndTurn,
        cancelled: false,
    };
    let mut tools: BTreeMap<usize, PendingTool> = BTreeMap::new();
    let mut started = false;
    loop {
        let next = tokio::select! {
            () = runtime.cancel.cancelled() => {
                model_call.cancelled = true;
                break;
            }
            result = tokio::time::timeout(STREAM_IDLE_TIMEOUT, futures_util::StreamExt::next(&mut stream)) => {
                match result {
                    Ok(next) => next,
                    Err(_) => {
                        return Err(StreamFailure {
                            started,
                            error: anyhow::anyhow!(
                                "model stream stalled for {}s",
                                STREAM_IDLE_TIMEOUT.as_secs()
                            ),
                        });
                    }
                }
            }
        };
        let Some(event) = next else {
            break;
        };
        let event = event.map_err(|error| StreamFailure { started, error })?;
        match event {
            StreamEvent::TextDelta(text) => {
                started = true;
                model_call.text.push_str(&text);
                runtime.emit(AgentEvent::TextDelta(text));
            }
            StreamEvent::ThinkingDelta(text) => {
                started = true;
                model_call.thinking.push_str(&text);
                runtime.emit(AgentEvent::ThinkingDelta(text));
            }
            StreamEvent::ThinkingSignature(signature) => {
                model_call.signature = Some(signature);
            }
            StreamEvent::ToolUseStart { index, id, name } => {
                started = true;
                let entry = tools.entry(index).or_default();
                if !id.is_empty() {
                    entry.id = id;
                }
                if !name.is_empty() {
                    entry.name = name;
                }
            }
            StreamEvent::ToolUseDelta {
                index,
                partial_json,
            } => {
                started = true;
                tools
                    .entry(index)
                    .or_default()
                    .arguments
                    .push_str(&partial_json);
            }
            StreamEvent::ToolUseEnd { .. } => {}
            StreamEvent::Done { stop_reason, usage } => {
                model_call.stop_reason = stop_reason;
                model_call.usage = usage;
                break;
            }
        }
    }

    for (index, pending) in tools {
        if pending.name.is_empty() {
            continue;
        }
        let id = if pending.id.is_empty() {
            format!("call_{index}")
        } else {
            pending.id
        };
        let raw = pending.arguments.trim();
        let (input, input_error) = if raw.is_empty() {
            (json!({}), None)
        } else {
            match serde_json::from_str::<Value>(raw) {
                Ok(value) if value.is_object() => (value, None),
                Ok(_) => (
                    json!({}),
                    Some("tool input must be a JSON object".to_string()),
                ),
                Err(error) => (json!({}), Some(error.to_string())),
            }
        };
        model_call.tool_calls.push(ToolCall {
            id,
            name: pending.name,
            input,
            input_error,
        });
    }

    Ok(model_call)
}

fn tool_registry(runtime: &Arc<Runtime>, agent: &AgentDefinition) -> ToolRegistry {
    ToolRegistry::standard().filter(
        agent.allowed_tools.as_deref(),
        &agent.denied_tools,
        &runtime.config.disabled_tools,
    )
}

async fn session_usage_with(runtime: &Arc<Runtime>, turn: &Usage) -> Usage {
    let session = runtime.session.lock().await;
    let mut total = session.usage;
    total.add(*turn);
    total
}

async fn session_cost_with(runtime: &Arc<Runtime>, turn_cost: f64) -> f64 {
    let session = runtime.session.lock().await;
    ((session.cost_usd + turn_cost) * 1_000_000.0).round() / 1_000_000.0
}

/// Called at the start of a turn after a resume or model switch. Older history
/// is replaced by a compact, deterministic handoff brief and the repository map
/// is refreshed by the system prompt. This avoids replaying large tool output to
/// a newly selected model while preserving the task state and recent turns.
async fn prepare_context_handoff(runtime: &Arc<Runtime>) -> Result<bool> {
    let pending = {
        let mut session = runtime.session.lock().await;
        session.take_context_handoff()
    };
    if !pending {
        return Ok(false);
    }
    if !runtime.config.context.resume_handoff {
        runtime.save_session().await?;
        return Ok(false);
    }

    let distilled = {
        let mut session = runtime.session.lock().await;
        apply_context_handoff(&mut session)
    };
    runtime.save_session().await?;
    match distilled {
        Some((removed, saved_chars)) => runtime.emit(AgentEvent::Notice(format!(
            "Context handoff: condensed {removed} older messages (~{} tokens saved) and refreshed the repository map.",
            saved_chars / 4
        ))),
        None => runtime.emit(AgentEvent::Notice(
            "Context handoff: refreshed the repository map for this session.".into(),
        )),
    }
    Ok(true)
}

fn apply_context_handoff(session: &mut Session) -> Option<(usize, usize)> {
    let cutoff = handoff_cutoff(&session.messages)?;
    let previous_chars = session.messages[..cutoff]
        .iter()
        .map(Message::estimated_chars)
        .sum::<usize>();
    let digest = render_handoff_digest(session, cutoff);
    let digest_chars = digest.len();
    if digest_chars >= previous_chars {
        return None;
    }
    let removed = cutoff;
    session.messages.drain(..cutoff);
    session.messages.insert(0, Message::user_text(digest));
    remap_turn_indices(&mut session.turns, removed, 1);
    session.updated_at = chrono::Utc::now();
    Some((removed, previous_chars - digest_chars))
}

fn handoff_cutoff(messages: &[Message]) -> Option<usize> {
    let user_turns = messages
        .iter()
        .enumerate()
        .filter(|(_, message)| is_plain_user_message(message))
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    if user_turns.len() <= HANDOFF_KEEP_TURNS {
        return None;
    }
    let cutoff = user_turns[user_turns.len() - HANDOFF_KEEP_TURNS];
    (cutoff > 0).then_some(cutoff)
}

fn render_handoff_digest(session: &Session, cutoff: usize) -> String {
    let mut out = String::new();
    out.push_str("[Context handoff — continue from here]\n");
    out.push_str(&format!(
        "Task: {}\nModel: {}/{}\n",
        session.display_title(),
        session.provider,
        session.model
    ));
    if let Some(stop) = &session.last_stop_reason {
        out.push_str(&format!("Last stop reason: {stop}\n"));
    }
    if !session.todos.is_empty() {
        out.push_str("\nTodos:\n");
        for todo in session.todos.iter().take(30) {
            out.push_str(&format!("- {} {}\n", todo.status.symbol(), todo.content));
        }
    }
    if !session.files_changed.is_empty() {
        out.push_str("\nFiles changed so far:\n");
        for path in session.files_changed.iter().take(80) {
            out.push_str(&format!("- {path}\n"));
        }
    }
    out.push_str("\nWork so far:\n");
    for message in &session.messages[..cutoff] {
        let role = match message.role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
        };
        out.push_str(&format!("\n### {role}\n"));
        for block in &message.content {
            match block {
                ContentBlock::Text { text } => {
                    if !text.trim().is_empty() {
                        out.push_str(&truncate(text.trim(), 1_200));
                        out.push('\n');
                    }
                }
                // Thinking blocks are provider-specific and expensive to replay.
                ContentBlock::Thinking { .. } => {}
                ContentBlock::Image { .. } => out.push_str("[image omitted]\n"),
                ContentBlock::ToolUse { name, input, .. } => {
                    out.push_str(&format!(
                        "[tool call {name}] {}\n",
                        truncate(&input.to_string(), 400)
                    ));
                }
                ContentBlock::ToolResult {
                    content, is_error, ..
                } => {
                    out.push_str(&format!(
                        "[tool result{}] {}\n",
                        if *is_error { " error" } else { "" },
                        truncate(content, 800)
                    ));
                }
            }
        }
    }
    out.push_str(
        "\nOlder tool output and images were intentionally omitted to save tokens. Re-read current files when exact contents matter.\n",
    );
    truncate(&out, HANDOFF_MAX_CHARS)
}

fn remap_turn_indices(turns: &mut [crate::session::TurnRecord], removed: usize, inserted: usize) {
    for turn in turns {
        if turn.prompt_index >= removed {
            turn.prompt_index = turn.prompt_index - removed + inserted;
        } else {
            turn.prompt_index = 0;
        }
    }
}

fn is_plain_user_message(message: &Message) -> bool {
    message.role == Role::User
        && message
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::Text { text } if !text.trim().is_empty()))
        && !message
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::ToolResult { .. }))
}

async fn maybe_compact(runtime: &Arc<Runtime>) -> Result<Option<(Usage, f64)>> {
    if !runtime.config.context.auto_compact {
        return Ok(None);
    }
    let (estimated, limit) = {
        let session = runtime.session.lock().await;
        (session.estimated_context_tokens(), runtime.context_limit())
    };
    let threshold = f64::from(runtime.config.context.compact_threshold).clamp(0.2, 0.95);
    if limit == 0 || (estimated as f64) < limit as f64 * threshold {
        return Ok(None);
    }
    let mut hook = runtime.hook_context(HookKind::PreCompact).await;
    hook.extra
        .insert("estimated_tokens".into(), json!(estimated));
    let outcome = runtime.run_hook(HookKind::PreCompact, &hook).await;
    emit_hook_messages(runtime, &outcome);
    if outcome.blocked {
        runtime.emit(AgentEvent::Warning(format!(
            "automatic compaction skipped by PreCompact hook{}",
            outcome
                .reason
                .map(|reason| format!(": {reason}"))
                .unwrap_or_default()
        )));
        return Ok(None);
    }
    runtime.emit(AgentEvent::CompactionStarted);
    match compact(runtime).await {
        Ok((removed, usage, cost)) => {
            runtime.emit(AgentEvent::CompactionFinished {
                removed_messages: removed,
            });
            let mut hook = runtime.hook_context(HookKind::PostCompact).await;
            hook.extra.insert("removed_messages".into(), json!(removed));
            let outcome = runtime.run_hook(HookKind::PostCompact, &hook).await;
            emit_hook_messages(runtime, &outcome);
            Ok(Some((usage, cost)))
        }
        Err(error) => {
            runtime.emit(AgentEvent::Warning(format!(
                "automatic compaction failed: {error:#}"
            )));
            Ok(None)
        }
    }
}

async fn compact(runtime: &Arc<Runtime>) -> Result<(usize, Usage, f64)> {
    let (messages, provider_name, model_name) = {
        let session = runtime.session.lock().await;
        (
            session.messages.clone(),
            session.provider.clone(),
            session.model.clone(),
        )
    };
    if messages.len() <= SUMMARY_KEEP_MESSAGES + 1 {
        return Ok((0, Usage::default(), 0.0));
    }
    let cutoff = messages
        .iter()
        .enumerate()
        .skip(messages.len().saturating_sub(SUMMARY_KEEP_MESSAGES))
        .find_map(|(index, message)| is_plain_user_message(message).then_some(index));
    let Some(cutoff) = cutoff else {
        return Ok((0, Usage::default(), 0.0));
    };
    if cutoff == 0 {
        return Ok((0, Usage::default(), 0.0));
    }
    let mut transcript = String::new();
    for message in &messages[..cutoff] {
        transcript.push_str(&format!("## {}\n", message.role));
        for block in &message.content {
            match block {
                ContentBlock::Text { text } => {
                    transcript.push_str(text.trim());
                    transcript.push('\n');
                }
                ContentBlock::Thinking { .. } => {}
                ContentBlock::Image { .. } => transcript.push_str("[image]\n"),
                ContentBlock::ToolUse { name, input, .. } => {
                    transcript.push_str(&format!(
                        "[tool {name}] {}\n",
                        truncate(&input.to_string(), 2_000)
                    ));
                }
                ContentBlock::ToolResult {
                    content, is_error, ..
                } => {
                    transcript.push_str(&format!(
                        "[tool result{}] {}\n",
                        if *is_error { " error" } else { "" },
                        truncate(content, 3_000)
                    ));
                }
            }
        }
        transcript.push('\n');
    }
    if transcript.len() > 400_000 {
        let chars = transcript.chars().count();
        transcript = transcript.chars().skip(chars - 400_000).collect();
    }
    let request_messages = vec![Message::user_text(format!(
        "Summarize the following coding-agent conversation so work can continue without the original messages. Preserve: the user's goal and constraints, decisions, files inspected or changed, commands run and their outcomes, errors, todos, and next steps. Be dense and concrete; omit pleasantries.\n\n{transcript}"
    ))];
    let resolved = resolve_model(&runtime.config, Some(&model_name), Some(&provider_name))?;
    let provider = runtime.provider_runtime.build(&resolved)?;
    let request = ChatRequest {
        model: resolved.reference.model.clone(),
        system: "You compact agent conversation history. Output only the continuation brief."
            .into(),
        messages: request_messages,
        tools: Vec::new(),
        max_tokens: 3_000,
        temperature: Some(0.2),
        effort: Effort::Off,
        stream: true,
    };
    let (summary, usage) = summarize(provider.as_ref(), request).await?;
    let summary = summary.trim();
    if summary.is_empty() {
        bail!("the summarizer returned no text");
    }
    let cost = estimate_cost(
        &runtime.config,
        &resolved.reference.provider,
        &resolved.reference.model,
        &usage,
    );
    let removed = {
        let mut session = runtime.session.lock().await;
        let removed = cutoff;
        session.messages.drain(..cutoff);
        session.messages.insert(
            0,
            Message::user_text(format!(
                "[Earlier conversation compacted]\n\n{summary}\n\nContinue the task from this point. Re-read files if exact current contents matter."
            )),
        );
        remap_turn_indices(&mut session.turns, removed, 1);
        if runtime.config.sessions.auto_save {
            runtime.store.save(&session)?;
        }
        removed
    };
    Ok((removed, usage, cost))
}

async fn summarize(provider: &dyn Provider, request: ChatRequest) -> Result<(String, Usage)> {
    let mut stream = provider.stream_chat(request).await?;
    let mut text = String::new();
    let mut usage = Usage::default();
    loop {
        let next = tokio::time::timeout(
            STREAM_IDLE_TIMEOUT,
            futures_util::StreamExt::next(&mut stream),
        )
        .await
        .map_err(|_| anyhow::anyhow!("summarizer stream stalled"))?;
        let Some(event) = next else {
            break;
        };
        match event? {
            StreamEvent::TextDelta(delta) => text.push_str(&delta),
            StreamEvent::Done { usage: done, .. } => {
                usage = done;
                break;
            }
            _ => {}
        }
    }
    Ok((text, usage))
}

fn emit_hook_messages(runtime: &Arc<Runtime>, outcome: &crate::hooks::HookOutcome) {
    for message in &outcome.messages {
        runtime.emit(AgentEvent::Notice(format!("hook: {message}")));
    }
}

async fn build_system_prompt(
    runtime: &Arc<Runtime>,
    agent: &AgentDefinition,
    refresh_context: bool,
) -> String {
    let session = runtime.session_snapshot().await;
    let mode = runtime.permissions.mode();
    let instructions =
        memory::load_project_instructions(&runtime.config, &runtime.project_root, &runtime.paths);
    let memory = MemoryStore::new(&runtime.project_root, &runtime.paths)
        .load()
        .unwrap_or_default();
    // Keep the system prompt stable between turns so providers with prompt
    // caching can reuse the prefix. Git state and the repository map are
    // refreshed only at session start, resume, or a model switch.
    let git = if refresh_context {
        crate::runtime::git_output(&runtime.cwd(), &["status", "--short", "--branch"])
            .await
            .ok()
            .filter(|status| !status.trim().is_empty())
    } else {
        None
    };

    let mut prompt = String::new();
    prompt.push_str(
        "You are Aaykra Code, a native Rust agentic coding assistant. You work directly in the user's project and finish tasks end-to-end. Use tools instead of guessing; verify changes with builds, tests, and diagnostics. Keep diffs focused and never claim something works without checking. Ask the user only when a decision is genuinely ambiguous or destructive. Never expose secrets or credentials.\n\n",
    );
    if !agent.system_prompt.trim().is_empty() {
        prompt.push_str("# Your role\n\n");
        prompt.push_str(agent.system_prompt.trim());
        prompt.push_str("\n\n");
    }
    prompt.push_str(&format!(
        "# Environment\n- Working directory: {}\n- Platform: {} {}\n- Date: {}\n- Provider/model: {}/{}\n- Agent: {}\n- Permission mode: {} ({})\n",
        runtime.cwd().display(),
        std::env::consts::OS,
        std::env::consts::ARCH,
        chrono::Utc::now().format("%Y-%m-%d"),
        session.provider,
        session.model,
        agent.name,
        mode.label(),
        mode.description(),
    ));
    if let Some(status) = &git {
        prompt.push_str("\n# Git status\n```\n");
        prompt.push_str(&truncate(status, 3_000));
        prompt.push_str("\n```\n");
    }
    if mode == crate::types::PermissionMode::Plan {
        prompt.push_str(
            "\n# Plan mode\nYou are read-only. Investigate the code and produce a concrete implementation plan (files, changes, order, tests, risks). Do not edit files or run mutating commands. The tools enforce this boundary.\n",
        );
    }
    if !instructions.content.is_empty() {
        prompt.push_str("\n# Project instructions\n\n");
        prompt.push_str(&instructions.content);
        prompt.push('\n');
    }
    if !memory.trim().is_empty() {
        prompt.push_str("\n# Persistent memory\n\n");
        prompt.push_str(&truncate(memory.trim(), 12_000));
        prompt.push('\n');
    }
    prompt.push_str("\n# Skills\n");
    for skill in runtime.skills.all().iter().take(40) {
        prompt.push_str(&format!(
            "- {}: {}\n",
            skill.name,
            truncate(&skill.description, 140)
        ));
    }
    prompt.push_str("\n# Agents\n");
    for definition in runtime.agents.all().iter().take(40) {
        prompt.push_str(&format!(
            "- {}: {}\n",
            definition.name,
            truncate(&definition.description, 140)
        ));
    }
    if refresh_context && runtime.config.context.repo_map_files > 0 {
        let root = runtime.cwd();
        let cache = runtime.paths.cache.clone();
        let max_files = runtime.config.context.repo_map_files;
        let cache_ttl = Duration::from_secs(runtime.config.context.repo_map_cache_secs);
        let map = tokio::time::timeout(
            Duration::from_secs(8),
            tokio::task::spawn_blocking(move || {
                crate::repo_map::quick_overview_cached(
                    &root,
                    &cache,
                    max_files,
                    REPO_MAP_MAX_CHARS,
                    cache_ttl,
                )
            }),
        )
        .await;
        if let Ok(Ok(Ok(map))) = map
            && !map.trim().is_empty()
        {
            prompt.push_str("\n# Repository map\n\n");
            prompt.push_str(&truncate(&map, REPO_MAP_MAX_CHARS));
            prompt.push('\n');
        }
    }
    prompt.push_str(
        "\n# Working rules\n- Read files before editing them; use Edit/MultiEdit for precise changes.\n- Use Glob/Grep/RepoMap to locate code and Lsp for real compiler diagnostics.\n- Use TodoWrite for multi-step work and keep exactly one item in progress.\n- Checkpoints are automatic before edits; summarize what changed when done.\n- In plan mode, do not attempt modifications.\n- Delegate broad read-only exploration to the Agent tool when it saves context.\n- If a tool or permission is denied, adapt instead of repeating the same call.\n",
    );
    prompt
}

/// Run a subagent in an isolated session and return its final text.
pub async fn run_subagent(
    parent: &Arc<Runtime>,
    agent_name: &str,
    prompt: &str,
    description: &str,
) -> Result<RunOutcome> {
    if prompt.trim().is_empty() {
        bail!("subagent prompt must not be empty");
    }
    if parent.agent_depth >= parent.max_agent_depth {
        bail!(
            "subagent depth limit reached ({}); finish the remaining work in this agent",
            parent.max_agent_depth
        );
    }
    let agent = parent
        .agents
        .get(agent_name)
        .cloned()
        .with_context(|| format!("unknown agent `{agent_name}`"))?;
    let parent_session = parent.session_snapshot().await;
    let (provider, model) = if let Some(model) = &agent.model {
        let resolved = resolve_model(&parent.config, Some(model), None)?;
        (resolved.reference.provider, resolved.reference.model)
    } else {
        (
            parent_session.provider.clone(),
            parent_session.model.clone(),
        )
    };
    let mut session = Session::new(provider, model, parent.cwd());
    session.agent = agent.name.clone();
    session.permission_mode = agent.permission_mode.unwrap_or(parent.permissions.mode());
    session.name = Some(format!("subagent: {}", truncate(description, 60)));
    session.title = truncate(description, 80);
    session.parent_id = Some(parent_session.id.clone());
    session.metadata.insert("subagent".into(), "true".into());
    session
        .metadata
        .insert("parent_id".into(), parent_session.id.clone());

    let ui: Arc<dyn UiGateway> = Arc::new(SubagentUi {
        parent: parent.ui.clone(),
        label: agent.name.clone(),
    });
    let mut child_runtime = Runtime::new(
        parent.config.clone(),
        session.clone(),
        parent.store.clone(),
        ui,
        parent.skills.clone(),
        parent.agents.clone(),
        parent.hooks.clone(),
        parent.mcp.clone(),
        parent.lsp.clone(),
        parent.provider_runtime.clone(),
        parent.project_root.clone(),
        parent.agent_depth + 1,
    );
    child_runtime.max_agent_depth = parent.max_agent_depth;
    child_runtime.cancel = parent.cancel.clone();
    let child = Arc::new(child_runtime);

    parent.emit(AgentEvent::SubagentStarted {
        id: session.id.clone(),
        description: description.to_string(),
    });
    let result = run_turn(child.clone(), Message::user_text(prompt), agent.max_turns).await;
    match &result {
        Ok(outcome) => parent.emit(AgentEvent::SubagentFinished {
            id: session.id.clone(),
            success: true,
            summary: truncate(&outcome.text, 500),
        }),
        Err(error) => parent.emit(AgentEvent::SubagentFinished {
            id: session.id.clone(),
            success: false,
            summary: truncate(&format!("{error:#}"), 500),
        }),
    }
    result
}

struct SubagentUi {
    parent: Arc<dyn UiGateway>,
    label: String,
}

#[async_trait::async_trait]
impl UiGateway for SubagentUi {
    async fn authorize(&self, mut request: PermissionRequest) -> PermissionDecision {
        request.reason = format!("[{}] {}", self.label, request.reason);
        self.parent.authorize(request).await
    }

    async fn ask(&self, question: UserQuestion) -> Option<UserAnswer> {
        self.parent.ask(question).await
    }

    fn emit(&self, event: AgentEvent) {
        match event {
            AgentEvent::Notice(_) | AgentEvent::Warning(_) | AgentEvent::Error(_) => {
                self.parent.emit(event);
            }
            _ => {}
        }
    }
}

/// Summarize the session without running an agent turn. Used by `/compact`.
pub async fn compact_session(runtime: &Arc<Runtime>) -> Result<String> {
    runtime.emit(AgentEvent::CompactionStarted);
    match compact(runtime).await {
        Ok((removed, _, _)) => {
            runtime.emit(AgentEvent::CompactionFinished {
                removed_messages: removed,
            });
            Ok(format!(
                "Compacted {removed} messages; the summary is now the first message."
            ))
        }
        Err(error) => {
            runtime.emit(AgentEvent::Warning(format!("compaction failed: {error:#}")));
            Err(error)
        }
    }
}

/// A compact local context report for `/context`.
pub async fn context_report(runtime: &Arc<Runtime>) -> String {
    let session = runtime.session_snapshot().await;
    let estimated = session.estimated_context_tokens();
    let limit = runtime.context_limit();
    let percent = if limit == 0 {
        0.0
    } else {
        estimated as f64 / limit as f64 * 100.0
    };
    let mut lines = vec![
        format!("Context: ~{estimated} / {limit} tokens ({percent:.1}%)"),
        format!(
            "Messages: {} · turns: {} · tools changed {} files",
            session.messages.len(),
            session.turns.len(),
            session.files_changed.len()
        ),
        format!(
            "Usage: {} in / {} out · cost {}",
            crate::types::compact_number(session.usage.input_tokens),
            crate::types::compact_number(session.usage.output_tokens),
            crate::models::format_cost(session.cost_usd)
        ),
    ];
    if !session.todos.is_empty() {
        lines.push(format!("Todos: {} open", open_todos(&session.todos)));
    }
    if percent >= 75.0 {
        lines.push("Consider `/compact` or a smaller task scope.".into());
    }
    lines.join("\n")
}

fn open_todos(todos: &[TodoItem]) -> usize {
    todos
        .iter()
        .filter(|todo| {
            matches!(
                todo.status,
                crate::types::TodoStatus::Pending | crate::types::TodoStatus::InProgress
            )
        })
        .count()
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
    fn estimate_tokens_is_positive() {
        assert!(crate::models::estimate_tokens("hello") > 0);
    }

    #[test]
    fn truncate_handles_unicode() {
        assert_eq!(truncate("héllo", 4), "hél…");
    }

    #[test]
    fn context_handoff_keeps_recent_turns_and_remaps_undo_indices() {
        let mut session = Session::new("anthropic", "old-model", std::path::PathBuf::from("."));
        for index in 0..4 {
            session.begin_turn(Message::user_text(format!("goal {index}")));
            session.push_message(Message::assistant_text(format!("answer {index}")));
        }
        let cutoff = handoff_cutoff(&session.messages).expect("cutoff");
        assert_eq!(cutoff, 4, "the last two user turns should be kept");
        let (removed, saved) = apply_context_handoff(&mut session).expect("handoff");
        assert_eq!(removed, 4);
        assert!(saved > 0);
        assert_eq!(session.messages.len(), 5);
        let digest = session.messages[0].text();
        assert!(digest.contains("goal 0"));
        assert!(digest.contains("answer 1"));
        assert!(!digest.contains("goal 2"));
        assert!(session.messages[1].text().contains("goal 2"));
        assert!(session.messages[3].text().contains("goal 3"));
        assert_eq!(session.turns.last().unwrap().prompt_index, 3);
        let undone = session.undo_last_turn().unwrap();
        assert_eq!(undone.number, 4);
        assert_eq!(session.messages.len(), 3);
    }

    #[test]
    fn context_handoff_collapses_large_tool_output() {
        let mut session = Session::new("openai", "gpt", std::path::PathBuf::from("."));
        session.begin_turn(Message::user_text("first task"));
        session.push_message(Message::assistant(vec![
            ContentBlock::Text {
                text: "checked".into(),
            },
            ContentBlock::ToolUse {
                id: "call".into(),
                name: "Read".into(),
                input: json!({"path": "huge.txt"}),
            },
        ]));
        session.push_message(Message::tool_result("call", "X".repeat(20_000), false));
        session.begin_turn(Message::user_text("second task"));
        session.push_message(Message::assistant_text("second answer"));
        session.begin_turn(Message::user_text("third task"));
        session.push_message(Message::assistant_text("third answer"));
        let before = session
            .messages
            .iter()
            .map(Message::estimated_chars)
            .sum::<usize>();
        let (removed, saved) = apply_context_handoff(&mut session).expect("handoff");
        let after = session
            .messages
            .iter()
            .map(Message::estimated_chars)
            .sum::<usize>();
        assert_eq!(removed, 3);
        assert!(saved > 15_000, "saved only {saved} chars");
        assert!(after < before);
        assert!(session.messages[0].text().contains("[tool call Read]"));
    }

    #[test]
    fn assistant_message_preserves_thinking_first() {
        let call = ModelCall {
            provider: "p".into(),
            model: "m".into(),
            text: "hello".into(),
            thinking: "plan".into(),
            signature: Some("sig".into()),
            tool_calls: vec![ToolCall {
                id: "1".into(),
                name: "Read".into(),
                input: json!({"path": "a"}),
                input_error: None,
            }],
            usage: Usage::default(),
            stop_reason: StopReason::EndTurn,
            cancelled: false,
        };
        let message = assistant_message(&call).unwrap();
        assert!(matches!(message.content[0], ContentBlock::Thinking { .. }));
        assert!(matches!(message.content[1], ContentBlock::Text { .. }));
        assert!(matches!(message.content[2], ContentBlock::ToolUse { .. }));
    }
}
