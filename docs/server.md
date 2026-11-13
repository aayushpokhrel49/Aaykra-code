# HTTP/SSE server

`akc serve` exposes the same agent loop as the TUI and CLI over a local REST and
Server-Sent Events API. It is designed for editor extensions, remote terminals,
and CI services that want live events rather than one-shot JSON.

```bash
akc serve --host 127.0.0.1 --port 4096 --token "$AAYKRA_SERVER_TOKEN"
```

The API is mounted at both `/api` and `/v1`; `/v1` is an alias intended for
editor clients. `GET /health` is available without authentication for process
supervision. Everything under `/api` and `/v1` requires a bearer token when one
is configured:

```http
Authorization: Bearer <token>
```

`X-Aaykra-Token: <token>` is accepted as well. CORS is disabled by default; pass
`--cors` to allow browser clients from any origin and to answer preflight
requests. Tokens should still be sent as headers, not query parameters.

## Errors

All non-2xx JSON responses use:

```json
{
  "error": "human-readable message",
  "status": 404
}
```

Validation failures return `400`, unknown sessions/requests return `404`,
concurrent turns and invalid state transitions return `409`, and unexpected
failures return `500`.

## Discovery

| Method | Path | Description |
| --- | --- | --- |
| `GET` | `/api` | API index and streaming contract |
| `GET` | `/api/health` | Liveness, version, uptime |
| `GET` | `/api/info` | Workspace, defaults, provider names, counts, sandbox state |
| `GET` | `/api/config` | Effective config without secrets |
| `GET` | `/api/models` | Providers, models, readiness, defaults, fallbacks |
| `GET` | `/api/agents` | All agents |
| `GET` | `/api/agents/{name}` | One agent, including its system prompt |
| `GET` | `/api/skills` | All skills |
| `GET` | `/api/skills/{name}` | One skill (leading `/` accepted) |
| `GET` | `/api/mcp` | Configured MCP servers and connection status |
| `POST` | `/api/mcp/discover` | Discover MCP tools/resources/prompts |

`POST /api/mcp/discover` accepts `{}` for all servers or
`{"server":"filesystem"}` for one:

```bash
curl -s localhost:4096/api/mcp/discover \
  -H 'content-type: application/json' \
  -d '{"server":"filesystem"}'
```

## Sessions

| Method | Path | Description |
| --- | --- | --- |
| `GET` | `/api/sessions?limit=50&cwd=/path` | Recent session summaries |
| `POST` | `/api/sessions` | Create and save a session |
| `GET` | `/api/sessions/{reference}` | Full session and transcript |
| `PATCH` | `/api/sessions/{reference}` | Rename or change model/agent/mode |
| `DELETE` | `/api/sessions/{reference}` | Delete a session and its checkpoints |
| `POST` | `/api/sessions/{reference}/fork` | Fork history into a new session |
| `POST` | `/api/sessions/{reference}/turn` | Run one turn (alias of `/turns`) |
| `POST` | `/api/sessions/{reference}/turns` | Run one turn (SSE by default) |
| `POST` | `/api/sessions/{reference}/cancel` | Cancel the active turn |
| `POST` | `/api/sessions/{reference}/undo` | Restore the last turn’s files and messages |

`{reference}` may be a full id, an unambiguous id prefix, or a session name.
Session summaries are returned with full ids.

Create a session:

```bash
curl -s localhost:4096/api/sessions \
  -H 'content-type: application/json' \
  -d '{
    "name": "editor-session",
    "provider": "anthropic",
    "model": "claude-sonnet-4-5",
    "agent": "general",
    "permission_mode": "ask"
  }'
```

All fields are optional; omitted fields use the effective configuration. The
response is the full `Session` JSON with `id`, `cwd`, `title`, `provider`,
`model`, `agent`, `permission_mode`, messages, usage, and cost.

Update a session while no turn is active:

```bash
curl -s -X PATCH localhost:4096/api/sessions/editor-session \
  -H 'content-type: application/json' \
  -d '{"name":"renamed","model":"openai/gpt-5.1","permission_mode":"accept-edits"}'
```

The same `provider`, `model`, `agent`, and `permission_mode` fields can be sent
on a turn request, in which case they apply before that turn starts and persist
with the session. An empty `name` clears the name.

Fork:

```bash
curl -s -X POST localhost:4096/api/sessions/editor-session/fork \
  -H 'content-type: application/json' \
  -d '{"name":"experiment"}'
```

Undo restores all files captured by the latest turn's checkpoint before removing
messages and usage. It returns `409` if restoration was only partial; in that
case the conversation is intentionally left intact.

## Streaming a turn

`POST /api/sessions/{id}/turns` accepts:

```json
{
  "prompt": "Fix the failing test",
  "images": [
    {"media_type": "image/png", "data": "<base64 without data-URL prefix>"}
  ],
  "stream": true,
  "auto_approve": false,
  "max_turns": 40,
  "provider": "anthropic",
  "model": "claude-sonnet-4-5",
  "agent": "general",
  "permission_mode": "ask"
}
```

Only `prompt` is commonly required; a prompt may be empty if at least one image
is supplied. Image media types must start with `image/`. Requests may be up to
32 MiB.

Streaming is the default. Set `"stream": false` to run the turn to completion
and return the final result JSON instead. In non-streaming mode there is no
interactive side channel, so permission prompts are denied unless
`auto_approve` is true or the session's permission mode/ rules allow the action.

### SSE response

The response uses `Content-Type: text/event-stream`. Named SSE events make
filtering easy; the JSON payload also has a `type` field.

```text
event: session_started
data: {"type":"session_started","id":"…","title":"Fix the failing test"}

event: turn_started
data: {"type":"turn_started","turn":1}

event: text_delta
data: {"type":"text_delta","text":"I'll inspect "}

event: tool_started
data: {"type":"tool_started","id":"call_0","name":"Read","input":"{\"path\":\"src/lib.rs\"}"}

event: tool_finished
data: {"type":"tool_finished","id":"call_0","name":"Read","output":"src/lib.rs …","is_error":false}

event: usage
data: {"type":"usage","turn":{…},"session_usage":{…},"session_cost_usd":0.0123}

event: turn_finished
data: {"type":"turn_finished","turn":1,"stop_reason":"end_turn",…}

event: result
data: {"type":"result","session_id":"…","turn":1,"text":"…","stop_reason":"end_turn","usage":{…},"cost_usd":0.004,"session_usage":{…},"session_cost_usd":0.0123,"is_error":false,"cancelled":false}
```

A keep-alive comment is sent every 15 seconds. The stream ends after the
`result` event. If the agent fails after streaming has begun, `result` contains
`"is_error": true` and an `error` string, and a `run_failed` event is also
emitted. If the client disconnects, the active turn is cancelled and the session
is saved; reconnect with `GET /api/sessions/{id}` to load the resulting history.

Event names:

| Event | Meaning |
| --- | --- |
| `session_started` | Session id and current title |
| `turn_started` | A new user turn began |
| `text_delta` | Assistant text chunk |
| `thinking_delta` | Reasoning chunk (when supported) |
| `tool_started` | Tool invocation began |
| `tool_finished` | Tool result/error |
| `notice`, `warning`, `error` | Human-readable status |
| `todo_updated` | Full replacement todo list |
| `usage` | Per-turn and cumulative usage/cost |
| `compaction_started`, `compaction_finished` | Context compaction |
| `subagent_started`, `subagent_finished` | Subagent lifecycle |
| `permission_mode_changed` | Effective mode changed |
| `model_changed` | Fallback or model switch |
| `turn_finished` | Turn usage and stop reason |
| `run_failed` | The agent loop failed |
| `queued_user_message` | A prompt was queued by another component |
| `permission_requested`, `permission_resolved` | Permission side channel |
| `question_requested`, `question_resolved` | Ask-user side channel |
| `result` | Final result document; stream ends |

Consumers should ignore unknown event types for forward compatibility.

### Permissions over SSE

When a tool needs approval, the stream emits:

```text
event: permission_requested
data: {"type":"permission_requested","request":{
  "id":"0f0c…","tool":"Write","target":"src/main.rs",
  "reason":"replace a file","diff":"…","read_only":false,
  "risk":"medium","suggested_rule":"Write(src/main.rs)",
  "affected_paths":["/project/src/main.rs"]
}}
```

Answer with the flattened permission decision:

```bash
# Allow once
curl -i -X POST localhost:4096/api/permissions/0f0c… \
  -H 'content-type: application/json' \
  -d '{"decision":"allow"}'

# Allow for the rest of the session and remember the suggested rule
curl -i -X POST localhost:4096/api/permissions/0f0c… \
  -H 'content-type: application/json' \
  -d '{"decision":"allow_for_session","rule":"Write(src/**)"}'

# Deny with a reason
curl -i -X POST localhost:4096/api/permissions/0f0c… \
  -H 'content-type: application/json' \
  -d '{"decision":"deny","reason":"do not touch main.rs"}'
```

A successful answer returns `204 No Content`; a resolved event then appears on
the stream. Unknown or already-answered ids return `404`.

### Questions over SSE

`AskUserQuestion` is delivered as:

```text
event: question_requested
data: {"type":"question_requested","id":"…","question":{
  "question":"Which database?","header":"Database",
  "options":[{"label":"Postgres","description":"…"}, …],
  "multiple":false
}}
```

Answer with a selected option, a custom string, or `null` to skip:

```json
{"answer":{"kind":"selected","values":["Postgres"]}}
{"answer":{"kind":"custom","text":"SQLite in tests"}}
{"answer":null}
```

`POST /api/questions/{id}` returns `204` on success.

## Cancellation and concurrency

Only one turn may run per session. A second request while a turn is active
returns `409`. Cancel with:

```bash
curl -s -X POST localhost:4096/api/sessions/{id}/cancel
```

The active turn checks cancellation during model streaming and between tool
calls; pending permission and question waits are resolved immediately. The
session is saved with stop reason `cancelled`.

`--yes`/`auto_approve` should be reserved for isolated environments. Bypass mode
is even broader: it skips all policy checks, including sensitive-path protection.

## Editor integration recipe

1. Start `akc serve` as a child process on an ephemeral localhost port.
2. Generate a random token and pass it via `--token` (or `AAYKRA_SERVER_TOKEN`).
3. Call `GET /v1/info` and `GET /v1/models` to populate settings.
4. Create or resume a session with `POST /v1/sessions` or `GET /v1/sessions/{id}`.
5. For each prompt, `POST /v1/sessions/{id}/turns` and parse SSE using a
   streaming HTTP client. Do not buffer the response.
6. Render `text_delta` incrementally; show `tool_started`/`tool_finished` cards.
7. On `permission_requested`, show a diff/approval UI and POST the decision.
8. On `question_requested`, show the options UI and POST the answer.
9. Keep the session id; reconnect by loading the session after a disconnect.
10. Expose undo, fork, model switching, and cancellation through the endpoints
    above instead of mutating files directly.

A small reference client lives in
[`integrations/aaykra-editor`](../integrations/aaykra-editor).

## Security notes

- Bind to `127.0.0.1` unless you intentionally need remote access.
- Always set `--token` for non-loopback binds. The server prints a warning if
  authentication is disabled on a non-loopback address.
- CORS is opt-in. When enabled, `Access-Control-Allow-Origin` is `*`; do not
  combine it with long-lived tokens on untrusted networks.
- The server has the same filesystem access as the user running it. Sandboxing
  applies to Bash tool execution, not to the HTTP listener itself.
