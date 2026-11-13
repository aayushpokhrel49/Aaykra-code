# Aaykra Code

**A fast, native, fully agentic coding agent written in Rust.**

Aaykra Code brings the workflow of Claude Code and OpenCode to a single, small
binary: a polished terminal UI, a scriptable headless mode, a local HTTP/SSE
server for editors, multi-provider streaming, real tools, checkpoints, and
sandboxed shell execution. No Node runtime, no Python environment, no hidden
daemon.

```console
$ akc "find the flaky test and fix it"
```

> Aaykra Code is pre-1.0. The CLI and server API are usable and tested, but
> configuration keys may still evolve between minor versions.

## Highlights

- **Native speed** — a pure-Rust core with a ratatui/crossterm TUI and a cold
  start that stays out of your way.
- **Every interface** — interactive TUI, one-shot CLI, JSON/stream-JSON for CI,
  and an HTTP/SSE server for editor integration.
- **Multi-provider** — Anthropic, OpenAI, Gemini, DeepSeek, Groq, OpenRouter,
  Together, Fireworks, Mistral, and any OpenAI-compatible endpoint, including
  Ollama, LM Studio, vLLM, llama.cpp, and llamafile.
- **Real agent loop** — streamed text and reasoning, native tool calls,
  fallback models, usage and cost tracking, automatic compaction, subagents,
  skills, hooks, MCP, and LSP diagnostics.
- **Safe by default** — Ask mode, path and shell rule matching, diff approval,
  checkpoints before every edit, and optional bubblewrap/seatbelt sandboxing.
- **Sessions that survive** — resume by id/name, continue the latest session for
  a directory, fork histories, and run parallel agents in git worktrees.
- **One-command undo** — restore every file changed in the previous turn and
  roll the conversation back with it.

## Install

Requirements: Rust 1.90 or newer and `git`. Optional: `bubblewrap` (Linux
sandbox), language servers, and `gh`/`glab` for pull-request workflows.

```bash
git clone https://github.com/aaykra/aaykra-code
cd aaykra-code
cargo install --path .
```

The package installs four equivalent entry points:

| Binary | Purpose |
| --- | --- |
| `aaykra-code` | Canonical executable |
| `akc` | Short alias |
| `aaykra-agent` | Explicit agent alias |
| `aaykra` | Supports `aaykra agent …`, `aaykra code …`, `aaykra tui …` |

## Quick start

```bash
# Store a provider key in ~/.aaykra/credentials.toml (mode 0600).
akc auth set anthropic

# Or use the environment:
export ANTHROPIC_API_KEY=sk-ant-...
# export OPENAI_API_KEY=... / DEEPSEEK_API_KEY=... / GEMINI_API_KEY=...

akc doctor                 # verify providers, tools, git, and sandbox support
akc init                   # write project config, AAYKRA.md, agent/skill dirs
akc                        # open the interactive TUI
akc tui                    # same, explicit
aaykra code                # same, from the aaykra alias
akc "explain this codebase"
```

> On a fresh machine with no credentials, the TUI opens an in-app setup
> screen first: pick a provider, paste a masked API key (stored in
> `~/.aaykra/credentials.toml`, mode 0600), or register a custom
> OpenAI-compatible endpoint — no shell commands needed.

Pick a provider or model explicitly:

```bash
akc --model anthropic/claude-sonnet-4-5
akc --provider openai --model gpt-5.1
akc --model ollama/qwen3-coder
```

## Interfaces

### Interactive TUI

Run `akc` in a terminal. The status line shows the active model, permission
mode, token usage, cost, context pressure, and turn state. The sidebar surfaces
context pressure, todos, changed files, and session state. When the active
provider needs credentials that are not configured, a setup screen is shown
before the main interface.

| Key | Action |
| --- | --- |
| `Enter` | Send the prompt |
| `Shift+Enter` / `Alt+Enter` | Insert a newline |
| `Shift+Tab` | Cycle Ask → Accept Edits → Plan → Auto → Bypass |
| `Ctrl+C` | Cancel the running turn (press `Ctrl+D` to quit) |
| `Ctrl+D` | Quit |
| `Ctrl+O` | Expand/collapse tool output |
| `Ctrl+T` | Toggle thinking visibility |
| `Ctrl+V` | Attach an image from the clipboard |
| `PageUp` / `PageDown` | Scroll the transcript |
| `Esc` | Cancel the running turn |
| `Ctrl+L` | Clear the visible transcript |

Permission prompts support `y` (allow once), `a` (allow for the session), and
`n`/`Esc` (deny). Questions support option selection, multi-select, and a
free-form “Other…” answer.

### Headless mode

Headless mode activates automatically when stdout is not a TTY, or explicitly
with `--print`/`run`:

```bash
akc --print "summarize src/agent.rs"
akc run --format json "list the risky unwraps"
akc run --format stream-json --yes "fix the failing test"
cat prompt.txt | akc run -q
```

Text mode streams the answer to stdout. `json` emits one final result object.
`stream-json` emits newline-delimited agent events followed by a result event,
which makes it easy to drive from CI or another process. Errors remain
machine-readable in both JSON modes.

### HTTP/SSE server

```bash
akc serve --host 127.0.0.1 --port 4096 --token "$AAYKRA_SERVER_TOKEN"
```

The same server powers editor integrations:

- `GET /api/sessions` and `POST /api/sessions` manage conversation state.
- `POST /api/sessions/{id}/turns` streams agent events over SSE.
- Permission prompts arrive as `permission_requested`; answer them with
  `POST /api/permissions/{id}`.
- `GET /api/models`, `/api/agents`, and `/api/skills` expose discovery.
- `/v1/…` is an alias of `/api/…` for editor clients.

See [docs/server.md](docs/server.md) for the complete API and
[integrations/aaykra-editor](integrations/aaykra-editor) for a minimal client.

## CLI reference

```
akc [OPTIONS] [PROMPT...] [COMMAND]

Commands:
  run       Run one prompt headlessly
  models    List configured providers and models
  sessions  List, inspect, rename, or delete sessions
  auth      Manage provider credentials
  mcp       Inspect MCP servers and discovery
  agents    List or inspect agents
  skills    List or inspect skills
  serve     Start the HTTP/SSE server
  init      Create config and project-instruction scaffolding
  diff      Show git status and diffs
  review    Run a read-only code review of the working tree
  doctor    Diagnose configuration, credentials, tools, and sandboxing
  config    Inspect configuration files and effective configuration
```

Frequently used global flags:

| Flag | Meaning |
| --- | --- |
| `-m, --model <MODEL>` | Model id or `provider/model` |
| `--provider <NAME>` | Provider override |
| `-a, --agent <NAME>` | Agent (`general`, `explore`, `plan`, `reviewer`, or custom) |
| `--permission-mode <MODE>` | `ask`, `accept-edits`, `plan`, `auto`, `bypass` |
| `--effort <LEVEL>` | `off`, `low`, `medium`, `high`, `max` |
| `--sandbox <MODE>` | `off`, `auto`, `bubblewrap`, `seatbelt` |
| `--network <BOOL>` | Allow network inside the sandbox |
| `-r, --resume <ID\|NAME>` | Resume a session |
| `-c, --continue` | Continue the latest session for this directory |
| `--fork` | Resume into a new session |
| `--name <NAME>` | Name the session |
| `-p, --print` | Force headless mode |
| `-q, --quiet` | Suppress progress on stderr |
| `--format <FORMAT>` | `text`, `json`, `stream-json` |
| `--image <PATH\|DATA_URL>` | Attach an image (repeatable) |
| `--no-tui` | Never start the TUI |
| `--yes` | Approve permission prompts (headless only) |

## Slash commands

Inside a session:

```
/help          /model [model]     /plan [on|off]   /compact
/context       /mcp [discover]    /agents [name]   /skills
/memory        /diff [path]       /code-review      /init
/undo          /clear             /permissions      /status
/name [name]   /sessions          /worktree         /quit
```

`/undo` restores files from the latest checkpoint and removes the corresponding
messages and usage. `/clear` resets the conversation while keeping checkpoint
lineage intact.

## Sessions and worktrees

Sessions live under `~/.aaykra/sessions/` as human-readable JSON. Resume the
latest session for the current directory with `akc -c`, or a specific one with
`akc --resume <id-or-name>`. `--fork` preserves the full history but starts a
fresh session id and checkpoint lineage.

Use `/worktree` (or the `EnterWorktree` tool) to give a parallel agent its own
git worktree, then `/worktree exit` to return. Background and parallel sessions
never share mutable working files unless you explicitly point them at the same
directory.

## Configuration

Precedence, lowest to highest:

1. Built-in defaults
2. `~/.aaykra/config.toml`
3. Project `.aaykra/config.toml`, then `aaykra.toml`
4. Explicit `--config <file>`
5. `AAYKRA_*` environment variables
6. CLI flags and in-session changes

Project instructions are loaded from `AAYKRA.md`, falling back to `CLAUDE.md`
or `AGENTS.md`. Global instructions live in `~/.aaykra/AAYKRA.md`. Persistent
memory is stored in `MEMORY.md` and injected into the system prompt.

See [docs/configuration.md](docs/configuration.md) and
[examples/config.toml](examples/config.toml) for a complete reference.

## Providers

Built-in provider profiles include Anthropic, OpenAI, Gemini, DeepSeek, Groq,
OpenRouter, Together, Fireworks, Mistral, Ollama, LM Studio, vLLM, llama.cpp,
and llamafile. Add any custom provider with an OpenAI-compatible protocol:

```toml
[providers.my-gateway]
protocol = "openai"
base_url = "https://gateway.example.com/v1"
api_key_env = "MY_GATEWAY_KEY"
default_model = "my-coder"
```

Credentials are resolved in this order: explicit config key, environment /
`${VAR}` expansion, then `~/.aaykra/credentials.toml`. The credential file is
written with mode 0600. Keys are never included in `/config` or server payloads.

Fallback models are tried when a request fails before any content has streamed:

```toml
fallback_models = ["openai/gpt-5.1", "deepseek/deepseek-chat"]
```

Switch models at any time with `/model`. The conversation, todos, changed files,
and recent turns are kept; before the next request, older work is condensed into
a portable handoff brief and the repository map is refreshed. Large old tool
output and images are dropped, so switching models does not replay the whole
transcript and does not lose the task state. Resume with `akc -c` or
`akc --resume` gets the same treatment. The system prompt stays stable between
turns for provider prompt caching, and repository maps are cached under
`~/.aaykra/cache/`; see [docs/configuration.md](docs/configuration.md#context-and-compaction).

## Tools

Aaykra Code ships with a focused, native tool set:

| Area | Tools |
| --- | --- |
| Files | `Read`, `Write`, `Edit`, `MultiEdit`, `Move`, `Delete` |
| Search | `Glob`, `Grep`, `RepoMap` (tree-sitter repository map) |
| Execution | `Bash`, `Git`, `Lsp`, `WebSearch`, `WebFetch` |
| Workflow | `TodoWrite`, `Agent`, `Skill`, `AskUserQuestion` |
| Modes | `EnterPlanMode`, `ExitPlanMode` |
| Parallelism | `EnterWorktree`, `ExitWorktree` |
| Safety | `Checkpoint`, `Memory` |
| Extensibility | `Mcp` (one deferred meta-tool for all configured servers) |

Every write, move, and delete is checkpointed before mutation. File edits show
a diff in the approval prompt. Shell commands are checked for chaining,
redirection, and destructive patterns; pipe-to-shell constructs are treated as
critical risk. Tool output is truncated with explicit notices so the model
knows what it did not see.

## Agents, skills, hooks, MCP, and LSP

- **Agents** are Markdown files with YAML frontmatter. Built-ins: `general`,
  `explore`, `plan`, and `reviewer`. Project agents live in
  `.aaykra/agents/`, global agents in `~/.aaykra/agents/`.
- **Skills** are Markdown workflows invoked with `/skill-name` or the `Skill`
  tool. Built-ins: `code-review`, `commit`, `debug`, and `security-review`.
- **Hooks** run shell commands on lifecycle events such as
  `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, and `Stop`. Blocking hooks can
  veto an action and feed a reason back to the agent.
- **MCP** supports stdio and remote Streamable HTTP servers, deferred tool
  loading, resource discovery, and prompts.
- **LSP** is a dependency-free JSON-RPC client with auto-detection for
  rust-analyzer, pyright/pylsp, typescript-language-server, gopls, clangd, and
  bash-language-server.

See [docs/extensibility.md](docs/extensibility.md).

## Security model

- Permission modes: **Ask** (default), **Accept Edits**, **Plan**, **Auto**, and
  **Bypass**. Plan hard-denies non-read-only tools.
- Rules can allow, ask, or deny by tool name, pattern, or shell prefix:
  `Read`, `Bash(git status)`, `Bash(git push:*)`.
- Paths outside the project and sensitive files (SSH keys, `.env`, cloud
  credentials, `.git` internals) require explicit approval.
- Checkpoints are written before any file mutation. Undo never silently deletes
  a file it cannot back up.
- The Bash tool uses bubblewrap on Linux and sandbox-exec on macOS when
  available. Network access is controlled by `sandbox.allow_network`.
- The HTTP server requires `Authorization: Bearer <token>` when started with
  `--token`. Binding to a non-loopback address without a token prints a loud
  warning.

## Development

```bash
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release
```

The crate forbids `unsafe` code and keeps the UI, agent loop, tools, and server
decoupled through the `UiGateway` interface. See
[docs/architecture.md](docs/architecture.md) for the module map.

## License

Apache-2.0.
