# Aaykra editor integration

This directory contains a small Rust reference bridge for embedding Aaykra Code
in an editor. The core agent remains the same Rust library used by `akc`; the
editor talks to it over the HTTP/SSE API.

The bridge is intentionally policy-safe: without `--yes` it denies permission
requests and skips questions. A real editor should replace those branches with
approval dialogs and option pickers.

## Run

Start the server:

```bash
akc serve --host 127.0.0.1 --port 4096 --token dev-token
```

Then, from this directory:

```bash
cargo run -- --server http://127.0.0.1:4096 --token dev-token \
  "explain the selected function"

# Continue a session:
cargo run -- --server http://127.0.0.1:4096 --token dev-token \
  --session <session-id> "now add a test"

# Approve permission prompts automatically (isolated environments only):
cargo run -- --server http://127.0.0.1:4096 --token dev-token --yes \
  "fix the failing test"
```

The bridge prints assistant text to stdout, progress/tool events to stderr, and
creates a new session when `--session` is omitted.

## Editor-side model

1. Spawn `akc serve` on an ephemeral localhost port with a random token. Keep the
   child process scoped to the editor workspace.
2. Query `GET /v1/info` and `GET /v1/models` for settings/defaults.
3. Create a session for the workspace or resume an existing session id.
4. For each prompt, open `POST /v1/sessions/{id}/turns` and consume the SSE
   stream incrementally.
5. Map `text_delta` to the editor transcript, `thinking_delta` to an expandable
   reasoning view, and tool events to activity cards.
6. On `permission_requested`, show the target, risk, and diff, then POST
   `/v1/permissions/{id}` with `allow`, `allow_for_session` plus a rule, or
   `deny`. The same permission object contains `suggested_rule`.
7. On `question_requested`, show options and POST `/v1/questions/{id}`.
8. Use `POST /v1/sessions/{id}/cancel` for a stop button, `/undo` for undo, and
   `/fork` for branching a conversation.
9. Reconnect after a dropped connection by loading the session JSON; the server
   cancels the active turn and saves the resulting transcript.

`GET /v1` returns a machine-readable endpoint index. The full protocol,
including every SSE event and response shape, is documented in
[`docs/server.md`](../../docs/server.md).

## Why an HTTP side channel?

SSE is one-way. Permissions and questions need a reply while the turn is still
streaming. The server associates each request with an id and holds the agent
future until the editor posts a decision. This avoids both auto-approving
dangerous actions and blocking the event stream while waiting.

If the editor disconnects, the pending decisions are cleaned up and the turn is
cancelled. No background agent is left waiting for an answer that will never
arrive.
