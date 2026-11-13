# Architecture

Aaykra Code is a library (`aaykra`) with four thin binary entry points:
`aaykra-code`, `akc`, `aaykra-agent`, and `aaykra`. The library is split so the
agent runtime can be embedded in editors, tests, and automation without going
through the CLI.

## Module map

| Module | Responsibility |
| --- | --- |
| `cli` | clap definitions, subcommands, headless execution, session preparation |
| `bootstrap` | `Services` construction, layered config, runtime/session wiring |
| `agent` | streaming turn loop, tool dispatch, fallbacks, compaction, subagents |
| `providers` | Anthropic, OpenAI-compatible, and Gemini streaming clients |
| `tools` | native tools, path normalization, authorization helpers, output truncation |
| `permissions` | rule compilation, risk classification, session rules |
| `runtime` | shared services, cancellation, authorization, checkpoints, worktrees |
| `session` | session model and JSON store |
| `checkpoint` | per-turn file manifests and restore |
| `commands` | slash commands shared by TUI, server-driven clients, and tests |
| `tui` | ratatui app, terminal lifecycle, gateway bridge, rendering |
| `ui/headless` | text, JSON, and stream-JSON gateways |
| `server` | axum REST/SSE API and permission/question side channel |
| `mcp` / `lsp` | deferred MCP client and dependency-free LSP JSON-RPC client |
| `repo_map` | tree-sitter repository map |
| `memory`, `skills`, `agents`, `hooks` | instructions, persistent memory, extensibility |

## Startup path

1. The CLI parses flags and loads layered configuration.
2. `Services::load_with` constructs the provider runtime, session store,
   permission-related config, hooks, skills, agents, MCP, and LSP managers.
3. A session is created or resolved from disk.
4. The selected UI gateway is constructed:
   - TUI: channel-backed gateway with ids for modal replies.
   - Headless: direct-output gateway; denies prompts unless `--yes`.
   - Server: SSE event sender plus pending permission/question maps.
5. A `Runtime` is built from the services and session.
6. `agent::run_turn` executes the turn.

All interfaces share the same agent loop, tools, permission engine, and session
format. There is no second implementation for headless mode.

## Turn loop

```text
run_turn
  ├─ UserPromptSubmit hook + optional automatic memory
  ├─ resolve agent and filtered tool registry
  ├─ session.begin_turn (persist immediately)
  ├─ build system prompt (instructions, repo map, skills, memory, todos)
  └─ run_loop
       ├─ cancellation check
       ├─ automatic compaction when above threshold
       ├─ call_model
       │    ├─ primary model
       │    ├─ stream text/thinking/tool calls
       │    └─ fallback models only before content starts
       ├─ persist assistant message
       ├─ execute tool calls sequentially
       │    ├─ PreToolUse hook
       │    ├─ authorize (policy → UI side channel)
       │    ├─ checkpoint before mutation
       │    ├─ execute + truncate output
       │    └─ PostToolUse hook
       └─ finish when no tool calls remain
  ├─ finish_turn (usage/cost/stop reason, save)
  └─ Stop hook
```

Tools run sequentially in the agent loop. This keeps permission prompts,
checkpoints, and transcript ordering predictable. Parallelism happens at a
higher level: subagents, separate sessions, and git worktrees.

## UI gateway

```rust
#[async_trait]
pub trait UiGateway: Send + Sync {
    async fn authorize(&self, request: PermissionRequest) -> PermissionDecision;
    async fn ask(&self, question: UserQuestion) -> Option<UserAnswer>;
    fn emit(&self, event: AgentEvent);
}
```

The agent never touches terminal state. `emit` is synchronous because streaming
deltas must not block the model. The TUI sends events through an unbounded
channel and uses oneshot channels for modal answers. The server emits SSE
events and registers pending decision senders. This is why permission handling
works identically in every front end.

`AgentEvent` has an explicit JSON serialization implementation rather than a
serde tagged derive. This keeps event names stable and avoids serde’s limitation
on internally tagged newtype variants with string payloads.

## Sessions and checkpoints

Sessions are pretty-printed JSON files under `~/.aaykra/sessions/`. Messages
store provider-neutral content blocks: text, thinking, images, tool uses, and
tool results. `Session` also tracks turns, usage, cost, todos, file changes, and
parent/worktree metadata.

Before a mutating action, the checkpoint store writes a manifest entry for the
path and a content backup under
`~/.aaykra/checkpoints/<session>/turn-<n>/backups/`. New files are recorded as
“did not exist”. `/undo` restores entries in reverse order and removes newly
created files. If any restore fails, the conversation is left untouched so the
agent and user can inspect the partial state.

## Subagents

`Agent` runs a new `Runtime` with a fresh session, a child depth, and the parent
cancellation token. The parent only receives notices and completion summaries;
the child’s transcript is saved separately with `parent_id` metadata. Built-in
`explore`, `plan`, and `reviewer` agents are read-only. Custom agents may use a
different model and tool subset.

## Server concurrency

The server owns a `Services` instance and allows one active turn per session.
Pending permission and question senders live in shared maps keyed by request id.
If the SSE connection drops, a drop guard cancels the turn; the model stream and
permission waits observe the cancellation token. The session is saved with a
`cancelled` stop reason so reconnecting clients can load the result.

## Performance choices

- Pure Rust, no runtime bridge or browser engine.
- Tokio for I/O; CPU-light code paths stay synchronous.
- `reqwest` + rustls with connection pooling and HTTP/2.
- Repository maps use tree-sitter only for supported languages; file search
  uses `ignore` and `globset`.
- The base prompt defers MCP tool schemas until the `Mcp` meta-tool is used.
- Tool output is truncated at explicit character/line limits.
- Four bins share one library and one dependency graph.

## Testing strategy

Unit tests cover config merge, permissions, checkpoints, frontmatter, provider
SSE parsing, session resolution, tools, and event serialization. Integration
tests exercise the CLI and server. For UI changes, run the TUI in a real
terminal; for protocol changes, stream a session with `curl -N` and answer a
permission request.
