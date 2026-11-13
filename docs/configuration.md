# Configuration reference

Aaykra Code merges configuration from several layers. Later layers override
earlier ones, key by key:

1. Built-in defaults
2. `~/.aaykra/config.toml` (or `$AAYKRA_HOME/config.toml`)
3. Project `.aaykra/config.toml`, then `aaykra.toml`
4. An explicit `--config <file>`
5. `AAYKRA_*` environment variables
6. CLI flags and in-session changes

Use `akc config path` to see which files were loaded, `akc config show` for the
effective configuration with credentials redacted, and `akc config template`
for a commented starting point.

## Locations

| Path | Purpose |
| --- | --- |
| `~/.aaykra/config.toml` | Global configuration |
| `.aaykra/config.toml` / `aaykra.toml` | Project configuration |
| `~/.aaykra/credentials.toml` | Stored API keys (mode 0600) |
| `~/.aaykra/sessions/` | Session JSON files |
| `~/.aaykra/checkpoints/` | Per-turn file backups |
| `~/.aaykra/skills/` | Global skills |
| `~/.aaykra/agents/` | Global agents |
| `~/.aaykra/AAYKRA.md` | Global instructions |
| `AAYKRA.md` / `CLAUDE.md` / `AGENTS.md` | Project instructions |
| `.aaykra/MEMORY.md` | Project persistent memory |

Set `AAYKRA_HOME` to relocate the global state directory. Set
`AAYKRA_GLOBAL_CONFIG` to point at a different global config file.

## Core settings

```toml
default_provider = "anthropic"
model = "anthropic/claude-sonnet-4-5"
fallback_models = ["openai/gpt-5.1", "deepseek/deepseek-chat"]

max_tokens = 8192
max_turns = 100
temperature = 0.2
effort = "medium"             # off | low | medium | high | max
permission_mode = "ask"       # ask | accept-edits | plan | auto | bypass
default_agent = "general"
auto_memory = true
instructions = ["docs/AGENT_NOTES.md"]
disabled_tools = ["WebSearch"]
```

`max_turns` limits model calls per user turn. `max_tokens` limits a single model
response. Fallback models are attempted only when a request fails before any
content has streamed, which avoids duplicating partial output.

## Providers

Built-in profiles include `anthropic`, `openai`, `gemini`, `deepseek`, `groq`,
`openrouter`, `together`, `fireworks`, `mistral`, `ollama`, `lmstudio`, `vllm`,
`llamacpp`, and `llamafile`. Each profile can be overridden or extended:

```toml
[providers.anthropic]
base_url = "https://api.anthropic.com"
default_model = "claude-sonnet-4-5"

[providers.openai]
base_url = "https://api.openai.com/v1"
default_model = "gpt-5.1"

# Any OpenAI-compatible endpoint:
[providers.gateway]
protocol = "openai"           # anthropic | openai | gemini
base_url = "https://gateway.example.com/v1"
api_key_env = "GATEWAY_API_KEY"
default_model = "my-coder"
known_models = ["my-coder", "my-coder-fast"]
requires_key = true
enabled = true

[providers.ollama]
base_url = "http://localhost:11434/v1"
default_model = "qwen2.5-coder:7b"
requires_key = false
```

Provider fields:

| Field | Meaning |
| --- | --- |
| `protocol` | Wire protocol: `anthropic`, `openai`, or `gemini` |
| `base_url` | API base URL |
| `api_key` | Inline key (prefer env or `akc auth set`) |
| `api_key_env` | Environment variable holding the key |
| `default_model` | Model used when none is supplied |
| `known_models` | Suggestions for `/model` and `akc models` |
| `headers` | Extra request headers |
| `enabled` | Disable a built-in profile with `false` |
| `requires_key` | Set `false` for local servers |
| `max_tokens_field` | Override the output-token request field |
| `extra_body` | Extra JSON merged into each request body |

### Credential sources

A key is resolved from, in order: inline `api_key`, `api_key_env` /
`${VAR}` expansion, then `~/.aaykra/credentials.toml` (written by
`akc auth set <provider>`). Keys are never printed by `config show` or the
server API.

### Provider environment variables

| Provider | Key | Base URL |
| --- | --- | --- |
| Anthropic | `ANTHROPIC_API_KEY` | `ANTHROPIC_BASE_URL` |
| OpenAI | `OPENAI_API_KEY` | `OPENAI_BASE_URL` |
| DeepSeek | `DEEPSEEK_API_KEY` | `DEEPSEEK_BASE_URL` |
| Groq | `GROQ_API_KEY` | `GROQ_BASE_URL` |
| Gemini | `GEMINI_API_KEY` or `GOOGLE_API_KEY` | `GEMINI_BASE_URL` |
| OpenRouter | `OPENROUTER_API_KEY` | `OPENROUTER_BASE_URL` |
| Together | `TOGETHER_API_KEY` | `TOGETHER_BASE_URL` |
| Fireworks | `FIREWORKS_API_KEY` | `FIREWORKS_BASE_URL` |
| Mistral | `MISTRAL_API_KEY` | `MISTRAL_BASE_URL` |

Additional `AAYKRA_*` variables:

| Variable | Meaning |
| --- | --- |
| `AAYKRA_MODEL` | Default model (`AKC_MODEL` also accepted) |
| `AAYKRA_PROVIDER` | Default provider |
| `AAYKRA_PERMISSION_MODE` | Permission mode |
| `AAYKRA_MAX_TOKENS` | Output token limit |
| `AAYKRA_EFFORT` | Reasoning effort |
| `AAYKRA_FALLBACK_MODELS` | Comma-separated fallback model list |
| `AAYKRA_HOME` | Relocate global state |
| `AAYKRA_GLOBAL_CONFIG` | Alternate global config path |
| `AAYKRA_SERVER_TOKEN` | Default bearer token for `akc serve` |

## Permissions

```toml
[permissions]
allow = ["Read", "Glob", "Grep", "Bash(git status)", "Bash(git diff:*)"]
ask = ["Bash(git push:*)", "Write(src/**)", "Edit(src/**)"]
deny = ["Bash(rm -rf /:*)", "Bash(curl * | sh:*)"]
additional_directories = ["../shared-library"]
```

Rule syntax:

- `Tool` — every invocation of that tool.
- `Tool(pattern)` — glob match against the target (path, command, URL, …).
- `Tool:prefix:*` — target/prefix match, useful for commands.

Rule resolution is deny → ask → allow. Tool matching is case-insensitive and
ignores `_`, `-`, and spaces. Glob rules also match affected paths and file
names. Session-scoped rules are added when the user chooses “allow for this
session”; they do not modify config files.

Permission modes:

| Mode | Behavior |
| --- | --- |
| `ask` | Every side effect asks. Safe default. |
| `accept-edits` | File edits inside the project are accepted automatically. |
| `plan` | Read-only; non-read-only tools are denied. |
| `auto` | Safe actions are allowed; risky actions ask. |
| `bypass` | No checks. Dangerous; intended only for isolated CI. |

Paths outside the project and sensitive files always require a decision unless a
rule or an explicit bypass mode overrides policy. Shell command classification
treats chaining/redirection as caution, destructive commands as high risk, and
pipe-to-shell as destructive.

## Sandbox

```toml
[sandbox]
mode = "auto"                 # off | auto | bubblewrap | seatbelt
allow_network = true
extra_writable = ["/tmp/build-cache"]
extra_readonly = ["/data/datasets"]
```

`auto` uses bubblewrap on Linux and sandbox-exec on macOS when the binary is
available, otherwise it runs commands without an OS sandbox while retaining
permission checks. `bubblewrap` and `seatbelt` fail closed if the sandbox helper
is unavailable.

## Context and compaction

```toml
[context]
window = 0                    # 0 = infer from the model
compact_threshold = 0.82
auto_compact = true
resume_handoff = true         # compact old work + refresh context on resume/model switch
max_tool_output_chars = 60000
max_tool_output_lines = 2000
repo_map_files = 1500
repo_map_cache_secs = 300     # 0 disables the repo-map cache
auto_diagnostics = true
deferred_mcp_tools = true
```

When estimated context crosses the threshold, older messages are summarized and
replaced with a compact system note. `/compact` forces the same operation.
Truncated tool output includes a notice with the truncation limits.

When a session is resumed or its model changes, `resume_handoff` condenses work
older than the last two turns into a deterministic handoff brief. The brief keeps
the goal, todos, changed files, decisions, and recent tool activity while
dropping large tool output, old images, and provider-specific thinking blocks.
The repository map is refreshed at the same time, so a new model starts from the
same codebase context without replaying the entire transcript. This is a local
transformation; it does not spend tokens on a summarizer call.

Repository maps are cached under `~/.aaykra/cache/repo-map/` for
`repo_map_cache_secs`. Set it to `0` to always rebuild. The cache only affects the
system-prompt overview; the `RepoMap` tool always reads the current tree.

The system prompt is deliberately stable between turns. Git status and the
repository map are refreshed only on the first turn, resume, or model switch,
which keeps provider prompt-cache prefixes reusable. The agent can run the Git
tool at any time for current status and diffs.

## Sessions and checkpoints

```toml
[checkpoints]
enabled = true
keep_turns = 50

[sessions]
auto_save = true
retention_days = 90
```

Sessions are saved as pretty JSON under `~/.aaykra/sessions/`. Checkpoints store
one manifest per turn plus content backups under
`~/.aaykra/checkpoints/<session>/turn-<n>/`. `/undo` restores files before
removing messages; if restoration fails, the conversation is kept so no state is
silently lost.

## Web tools

```toml
[web]
search_provider = "duckduckgo"  # duckduckgo | tavily | brave
max_results = 8
fetch_max_bytes = 2000000
# tavily_api_key = "..."
# brave_api_key = "..."
```

`WebFetch` converts HTML to text and enforces the byte limit. `WebSearch`
requires no key for DuckDuckGo; Tavily and Brave read their configured keys.

## Git

```toml
[git]
auto_commit = false
commit_style = "conventional"
create_pr = false
protected_branches = ["main", "master"]
```

`auto_commit` is off by default. Commits and pushes still pass through the
permission engine, and protected branches are surfaced by the Git tool helpers.

## UI

```toml
[ui]
show_thinking = false
show_tool_output = false
compact_tools = true
mouse = true
animations = true
theme = "aaykra"
```

The TUI can toggle tool expansion with `Ctrl+O`. Reasoning deltas are only shown
when `show_thinking` is enabled.

## MCP

```toml
[mcp.filesystem]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "."]
timeout_ms = 20000

[mcp.remote]
url = "https://mcp.example.com/mcp"
headers = { Authorization = "Bearer ${MCP_TOKEN}" }
enabled = true
```

A server needs either `command` (stdio) or `url` (remote HTTP). Tool discovery is
deferred until the agent invokes the `Mcp` meta-tool, keeping the base prompt
small. `akc mcp status` and `akc mcp discover [server]` inspect configured
servers without starting an interactive session.

## LSP

```toml
[lsp.rust]
command = "rust-analyzer"
args = []
extensions = ["rs"]

[lsp.python]
command = "pylsp"
extensions = ["py"]
```

`extensions` may be written with or without a leading dot. When no config is
present, common language servers are auto-detected on `PATH`. Diagnostics,
definitions, references, and hover are available through the `Lsp` tool.

## Hooks

```toml
[[hooks.pre_tool_use]]
command = "scripts/check-tool.sh"
matcher = "Bash|Write|Edit"
timeout_ms = 30000
blocking = true
```

Hook keys are lifecycle events such as `user_prompt_submit`, `pre_tool_use`,
`post_tool_use`, and `stop`. `matcher` is a regular expression matched against
the tool name (tool hooks) or prompt (prompt hooks). A nonzero exit from a
`blocking` hook vetoes the action and its stderr is shown to the agent.
