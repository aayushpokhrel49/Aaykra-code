# Extensibility: agents, skills, hooks, MCP, and LSP

## Custom agents

Agents are Markdown files in `.aaykra/agents/` (project) or
`~/.aaykra/agents/` (global). Project agents override global agents, which
override built-ins of the same name. The filename is used when `name` is
omitted.

```markdown
---
name: test-runner
description: Runs tests and diagnoses failures with minimal fixes.
tools: Read, Grep, Glob, Bash, Lsp
disallowed_tools: Write, Delete
model: anthropic/claude-sonnet-4-5
max_turns: 30
permission_mode: ask
mcp_servers: []
---

You are a test specialist. Run the narrowest relevant command first…
```

Frontmatter fields:

| Field | Meaning |
| --- | --- |
| `name` | Agent id (lowercase, letters/digits/`-`/`_`) |
| `description` | One-line catalog description |
| `tools` / `allowed_tools` | Comma-separated allow-list; omitted means all |
| `disallowed_tools` / `denied_tools` | Comma-separated deny-list |
| `model` | Optional override (`provider/model` or a model id) |
| `max_turns` | Maximum model calls for each delegated run |
| `permission_mode` | `ask`, `accept-edits`, `plan`, `auto`, `bypass` |
| `mcp_servers` | Restrict visible MCP servers; empty list means none |

The body is the agent's system prompt. Built-ins:

- `general` — full-capability implementation agent.
- `explore` — read-only codebase scout with a focused context window.
- `plan` — read-only architect producing implementation plans.
- `reviewer` — read-only senior reviewer focused on real defects.

Invoke an agent with `--agent`, `/agents <name>`, or the `Agent` tool from the
parent agent. Subagents cannot ask follow-up questions; their prompt must be
self-contained.

## Skills

Skills are reusable workflows. A skill can be a single Markdown file or a
directory containing `SKILL.md`. Project skills live in
`.aaykra/skills/`, global skills in `~/.aaykra/skills/`.

```markdown
---
name: deploy-staging
description: Build, verify, and deploy the current branch to staging.
allowed_tools: Bash, Read, Git
---

1. Verify the branch and working tree are clean.
2. Run the documented build and test commands.
3. Build the deployment artifact.
4. Deploy only after all checks pass.
```

| Field | Meaning |
| --- | --- |
| `name` | Skill id; invoked as `/name` |
| `description` | Catalog description |
| `allowed_tools` / `tools` | Suggested tools (displayed in the prompt) |

The body is injected as workflow instructions. Built-in skills: `code-review`,
`commit`, `debug`, and `security-review`. Invoke a skill with `/name` in a
prompt or through the `Skill` tool. A skill is guidance, not a privilege
escalation: every underlying tool call still goes through permissions.

## Hooks

Hooks are shell commands attached to lifecycle events. They receive a JSON
payload on stdin and also get `AAYKRA_*` environment variables.

```toml
[[hooks.pre_tool_use]]
command = "scripts/guard.sh"
matcher = "Bash|Write|Edit"
timeout_ms = 30000
blocking = true
```

Supported event keys:

| Key | When |
| --- | --- |
| `session_start` | Session begins |
| `user_prompt_submit` | Before a user prompt is accepted |
| `pre_tool_use` | Before a tool executes |
| `post_tool_use` | After a tool returns |
| `pre_compact` | Before context compaction |
| `post_compact` | After context compaction |
| `stop` | After a completed turn |
| `session_end` | Session ends |
| `notification` | Notification event |

Matching is case- and separator-insensitive, so `PreToolUse` and
`pre_tool_use` are equivalent. `matcher` is a Rust regular expression applied to
the tool name for tool hooks and the prompt for prompt hooks. A nonzero exit
from a `blocking` hook vetoes the action; stderr is surfaced as the reason.
Non-blocking hook failures are only reported. Timeouts are clamped to 1–300
seconds.

Payload fields include `hook_event_name`, `event`, `session_id`, `cwd`,
`provider`, `model`, `tool_name`, `tool_input`, `tool_output`, `tool_error`,
`prompt`, and `extra`.

Environment variables: `AAYKRA_HOOK_EVENT`, `AAYKRA_SESSION_ID`,
`AAYKRA_CWD`, `AAYKRA_PROVIDER`, `AAYKRA_MODEL`, `AAYKRA_TOOL_NAME`,
`AAYKRA_TOOL_INPUT`, `AAYKRA_TOOL_OUTPUT`, and `AAYKRA_USER_PROMPT` when
applicable.

## MCP servers

MCP is exposed to the model through one deferred `Mcp` meta-tool. This avoids
loading every server's schemas into the base prompt.

```toml
[mcp.filesystem]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "."]
timeout_ms = 20000

[mcp.docs]
url = "https://mcp.example.com/mcp"
headers = { Authorization = "Bearer ${MCP_TOKEN}" }
```

MCP actions:

| Action | Input | Result |
| --- | --- | --- |
| `list` | optional `server` | Discovered tools, resources, and prompts |
| `call` | `server`, `tool`, `arguments` | Tool result |
| `read_resource` | `server`, `uri` | Resource contents |
| `get_prompt` | `server`, `prompt`, `arguments` | Rendered prompt |

Stdio servers are spawned lazily; HTTP servers use Streamable HTTP with SSE
support. Calls are permission-checked (`Mcp(call:*)` is not read-only). Agent
definitions can restrict which servers a subagent may see.

Inspect without starting a session:

```bash
akc mcp status
akc mcp discover
akc mcp discover filesystem
```

## LSP servers

The LSP client speaks JSON-RPC over stdio and is dependency-free. Configure
servers explicitly or rely on auto-detection for common languages.

```toml
[lsp.rust]
command = "rust-analyzer"
args = []
extensions = ["rs"]

[lsp.typescript]
command = "typescript-language-server"
args = ["--stdio"]
extensions = ["ts", "tsx", "js", "jsx"]
```

The `Lsp` tool supports:

| Action | Required input | Purpose |
| --- | --- | --- |
| `diagnostics` | `path` | Wait for and return compiler diagnostics |
| `definition` | `path`, `line`, `column` | Go to definition |
| `references` | `path`, `line`, `column` | Find references |
| `hover` | `path`, `line`, `column` | Hover/type information |

Lines and columns are 1-based. Servers are started lazily and scoped to the
project root. `context.auto_diagnostics` enables diagnostics after relevant file
changes when a server is available.

## Built-in slash commands for extensibility

- `/agents [name]` lists agents or switches the active agent.
- `/skills` lists discovered skills.
- `/mcp [status|discover|server…]` inspects MCP.
- `/memory [add|add-global|remove] [text]` edits persistent memory.
- `/init` generates `AAYKRA.md` using repository exploration.
