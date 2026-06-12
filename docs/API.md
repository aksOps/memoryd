# memoryd API

This document describes the current external interfaces. The implementation is
early and intentionally narrow.

## CLI

### `doctor`

```bash
memoryd doctor [--db <path>] [--bind <addr:port>] [--token <token>] [--token-file <path>]
```

Checks SQLite schema, WAL mode, foreign keys, and configuration safety.

### `stats`

```bash
memoryd stats [--db <path>] [--bind <addr:port>] [--token <token>] [--token-file <path>]
```

Prints row counts for canonical tables.

### `remember`

```bash
memoryd remember <content> [--kind <kind>] [--session <id>] [--source <source>] [--tags <a,b>] [--db <path>]
```

Writes a memory capture through the same append-only path as HTTP capture. It
redacts common secret shapes before persistence, normally returns after the raw
event and one embed job are persisted, writes capture/redaction audit rows, and
does not call a provider inline.

If the configured queue-depth cap is reached, capture still persists the raw
event and returns a degraded response with no queued embed job.

Output JSON:

```json
{
  "raw_event_id": 1,
  "session_id": "cli",
  "enqueued_job_id": 1,
  "pending_memory": true,
  "degraded": false
}
```

### `recall`

```bash
memoryd recall <query> [--k <limit>] [--db <path>]
```

Runs local lexical recall over redacted captured raw events using SQLite FTS. It
does not call a provider inline.

Output JSON:

```json
{
  "results": [
    {
      "raw_event_id": 1,
      "session_id": "session-1",
      "ts_ms": 1234,
      "source": "tool_result",
      "kind": "observation",
      "content": "WAL timeout fixed",
      "score": -0.000001
    }
  ],
  "degraded": false,
  "mode": "lexical"
}
```

### `import`

```bash
memoryd import --source jsonl|claude|codex|opencode|hermes|agents [--path <file|dir|db>] [--db <path>]
```

Backfills historic data through the same capture path as `remember` (redaction
included; no privileged path). Sources:

- `jsonl` — one generic JSON object per line with a required `text` field;
  `--path <file>` is required.
- `claude` / `codex` — native session transcripts, auto-discovered under
  `~/.claude/projects/*/*.jsonl` and `~/.codex/sessions/**/*.jsonl`; `--path`
  optionally points at one file or an exported directory instead.
- `opencode` / `hermes` — the agent's SQLite database
  (`$XDG_DATA_HOME/opencode/opencode.db`, `~/.hermes/state.db`), opened
  strictly read-only; `--path` optionally points at a copied database file or
  its directory.
- `agents` — every agent that `memoryd integrate` would detect, in one run.

Re-runs are idempotent (content-hash dedup); a full embed queue pauses the run
(`"state": "paused"`) and a re-run resumes it. Files over 64 MiB and files with
no importable content are skipped with a per-file `note`. Each unit is
role-prefixed (`[user]`/`[assistant]`/`[tool]`) and capped at 4,000 characters.

`--source jsonl` keeps the original single-batch response:

```json
{"batch_id":"…","source":"jsonl","path":"hist.jsonl","total":2,"processed":2,"skipped":0,"state":"completed"}
```

Single-agent sources aggregate one batch per discovered file:

```json
{
  "source": "claude-session",
  "total": 12,
  "processed": 12,
  "skipped": 0,
  "state": "completed",
  "batches": [
    {"path": "…/s1.jsonl", "batch_id": "…", "total": 8, "processed": 8, "skipped": 0, "state": "completed"},
    {"path": "…/s2.jsonl", "note": "skipped: no importable content"}
  ]
}
```

`--source agents` wraps one such object per detected agent:

```json
{
  "agents": [
    {"agent": "claude", "detected": true, "source": "claude-session", "total": 12, "processed": 12, "skipped": 0, "state": "completed", "batches": ["…"]},
    {"agent": "codex", "detected": false}
  ]
}
```

### `serve`

```bash
memoryd serve [--db <path>] [--bind <addr:port>] [--token <token>] [--token-file <path>]
```

Starts the local HTTP server. The default bind is `127.0.0.1:7077`. Any
non-loopback bind requires a bearer token at startup.

`--adapter <null|local|openai_compat>` (or `MEMORYD_ADAPTER`) selects the
provider. `openai_compat` is the single generic remote adapter — any
OpenAI-shaped endpoint via `MEMORYD_OPENAI_BASE_URL` (api.openai.com, Ollama's
`/v1`, vLLM, LM Studio) with `MEMORYD_OPENAI_API_KEY[_FILE]`,
`MEMORYD_OPENAI_EMBED_MODEL`, `MEMORYD_OPENAI_CHAT_MODEL`, and
`MEMORYD_OPENAI_USD_PER_1K`; it requires a non-zero `MEMORYD_SPEND_CAP_USD`.

### `tui`

```bash
memoryd tui [--db <path>]
```

Local-only, read-only interactive store viewer (requires stdout to be a
terminal). Five tabs — Memories, Sessions, Profile, Imports, Stats — browse
the store through the same read paths the CLI and MCP server use: paged
memory/session lists, `/` lexical search (the CLI recall path under the
`null` adapter — never embeds), Enter for a memory's graph neighborhood
(`g` re-centers on a neighbor), distilled session narratives, approved
profile facts, import-batch progress, and table counts plus db path/size.
No network bind, no provider calls, no writes beyond the access bookkeeping
recall already performs. Keys: `Tab`/`1-5` switch tabs, `j`/`k`/arrows move,
`Enter` open, `Esc` back, `q`/`Ctrl-C` quit.

## REST

Base URL: `http://127.0.0.1:7077` by default.

### `GET /v1/health`

Liveness probe. Read-only; reports nothing beyond status and schema version.

Loopback peers may call it without authorization even when a bearer token is
configured, so local supervisors can probe without holding the token. Non-
loopback peers go through normal bearer auth. Non-GET methods return `405`.

Response `200 OK`:

```json
{
  "status": "ok",
  "schema_version": 7
}
```

`schema_version` tracks the binary's current schema version (`SCHEMA_VERSION`
in `crates/memoryd-core/src/store.rs`); a newer binary reports a higher value
than this example.

### `POST /v1/capture`

Redacts common secret shapes, appends the redacted raw event, upserts its
session, normally enqueues one `embed` job, writes capture/redaction audit rows,
and returns immediately. The handler performs no provider calls.

When the configured queue-depth cap is reached, the handler still returns `202`
after appending the raw event, but `degraded` is `true` and `enqueued_job_id` is
`null` because no embed job was queued.

Request headers:

```http
Content-Type: application/json
Authorization: Bearer <token>
```

`Authorization` is required when a token is configured. Loopback calls may omit
authorization when no token is configured.

Requests must be framed with `Content-Length`; `Transfer-Encoding: chunked` is
not supported and is rejected with `501 not_implemented`. `ts_ms` (or `ts`)
must be integer milliseconds when present — any other JSON type, including a
numeric string, is rejected with `422`; omitting it uses the server clock.

Request body:

```json
{
  "session_id": "session-1",
  "agent": "claude",
  "source": "tool_result",
  "kind": "observation",
  "payload": { "text": "WAL timeout fixed" },
  "provenance": { "tags": ["db"] },
  "ts_ms": 1234
}
```

Response `202 Accepted`:

```json
{
  "raw_event_id": 1,
  "session_id": "session-1",
  "enqueued_job_id": 1,
  "degraded": false,
  "processed": false
}
```

Degraded `202 Accepted` response when the queue-depth cap is reached:

```json
{
  "raw_event_id": 2,
  "session_id": "session-1",
  "enqueued_job_id": null,
  "degraded": true,
  "processed": false
}
```

The persisted `session_id`, `agent`, `source`, `kind`, `payload`, `provenance`,
and recall index text are redacted before the SQLite transaction. If a metadata
field itself contains a bearer-style secret, the response reflects the redacted
stored value. Redaction replaces matched content with `[REDACTED]`.

Current redaction coverage is deterministic and best-effort: sensitive JSON keys,
bearer-style credentials, common API-key prefixes, private-key markers, emails,
and high-entropy token-like spans. It is not a proof that arbitrary proprietary
secret formats will always be detected.

Audit side effects:

- Successful capture appends `audit_log(action='capture.append')`.
- Captures with redactions also append `audit_log(action='redaction.apply')` with
  counts and the replacement marker only, not original secret material.
- HTTP auth rejection appends `audit_log(action='auth.reject')` with allow-listed
  method/path classes and booleans; bearer token values are not stored.

Error envelope:

```json
{
  "error": {
    "code": "invalid_request",
    "message": "payload is required"
  }
}
```

Current status codes: `400`, `401`, `404`, `405`, `408`, `413`, `415`, `422`,
`429`, `431`, `500`, `501`, and `503`.

### `POST /v1/recall`

Runs local lexical recall over redacted captured raw events. The handler performs
no provider calls and writes no provider usage rows.

Request body:

```json
{
  "query": "WAL timeout",
  "k": 5
}
```

Response `200 OK`:

```json
{
  "results": [
    {
      "raw_event_id": 1,
      "session_id": "session-1",
      "ts_ms": 1234,
      "source": "tool_result",
      "kind": "observation",
      "content": "WAL timeout fixed",
      "score": -0.000001
    }
  ],
  "degraded": false,
  "mode": "lexical"
}
```

Empty or punctuation-only queries return `422` with the standard error envelope.

## MCP (stdio)

`memoryd mcp [--db <path>]` runs an MCP server over stdio: newline-delimited
JSON-RPC 2.0, protocol revision `2024-11-05`. Supported lifecycle methods:
`initialize`, `notifications/initialized`, `ping`, `tools/list`, `tools/call`.
Resources are not exposed in this slice (capabilities declare only `tools`).

Trust model: the server reads stdin and writes stdout only — it never binds a
socket — so trust is inherited from the parent process that spawned it, the
same boundary as running the CLI. No bearer token applies. All diagnostics go
to stderr; stdout carries only JSON-RPC lines.

Captures made through `memory_remember` are acknowledged immediately and
consolidate into recallable durable memories on the next `serve` or `dream`
run; `mcp` mode runs no background workers itself.

Note on naming: ARCHITECTURE-PLAN §6.5 sketches dotted tool names
(`memory.remember`); the shipped names use underscores (`memory_remember`)
because MCP clients enforce `^[a-zA-Z0-9_-]{1,64}$` for tool names (§14.3).

### Tools

- `memory_remember` — `{content (required), kind = "note", session_id = "mcp",
  source = "mcp", tags []}`. Persists one capture through the normal redaction
  pipeline; returns `{raw_event_id, session_id, enqueued_job_id,
  pending_memory, degraded}`.
- `memory_recall` — `{query (required), k 1–50 = 5, semantic = false,
  hops 0|1 = 1}`. Durable-memory recall with one-hop graph expansion, falling
  back to raw-event lexical recall; same result shape as `POST /v1/recall`.
- `memory_stats` — `{}`. Row counts for every canonical table.
- `memory_profile` — `{limit 1-100 = 50}`. The owner's approved persona
  kernel: active profile facts and `heuristic.*` decision principles (all
  through the approvals gate) plus up to 12 top-centrality themes. Built for
  one-call persona loading at session start.
- `memory_graph` — `{memory_id (required), limit 1–50 = 16}`. Direct
  association-graph neighbors of a memory over `memory_links`, strongest link
  first, each with `link_type` (`semantic`, `co_occurrence`, ...),
  `link_strength`, `last_reinforced_at`, and a content snippet (240 chars).

Example call:

```json
{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"memory_graph","arguments":{"memory_id":"27938c38d764..."}}}
```

### Resources

`resources/list` returns `memory://profile` (the persona kernel as JSON) and
one `memory://session/{id}` entry per distilled session (newest 50);
`resources/read` serves them (`application/json` / `text/plain`). Unknown
URIs return `-32002` "resource not found". The capability set is
`{"tools": {}, "resources": {}}`.

### Error mapping

Protocol failures are JSON-RPC errors: `-32700` unparseable line, `-32600`
invalid request / oversized line, `-32601` unknown method, `-32602` unknown
tool or invalid tool arguments, `-32002` request before `initialized`,
`-32603` internal store error (generic message; detail goes to stderr only).

Execution failures are in-band tool results with `isError: true` so the
calling model can react: blank recall query ("query must contain searchable
text"), empty capture fields ("capture fields must not be empty"), and unknown
`memory_id` ("memory not found").

## Agent integration (`memoryd integrate`)

```bash
memoryd integrate [--agent claude|opencode|codex|hermes] [--scope user|project] [--mode mcp|hooks|all] [--dry-run] [--bin <path>] [--db <path>]
memoryd hook <tool|prompt|session-start> [--agent <label>] [--db <path>]
```

`--mode mcp` (default) registers the MCP server + a session-end dream hook.
`--mode hooks` skips MCP and installs the push-based suite instead — capture
hooks (`PostToolUse`/`post_tool_call` tool results, `UserPromptSubmit`
prompts) and context-injection hooks (`SessionStart` persona,
`UserPromptSubmit` recall) where the agent supports injection (Claude Code,
Codex). `--mode all` installs both. `memoryd hook` is the handler the
installed hooks invoke: stdin is the agent's hook payload JSON; stdout is
empty or a `hookSpecificOutput.additionalContext` envelope; it always exits 0
so a broken store can never block the host agent. Capture goes through the
normal redaction pipeline with text capped at 4000 chars; injected context is
capped at 2000 chars and recall runs locally (no provider calls).

Auto-discovers installed agents (by their config dir under `$HOME`) and
registers the memoryd MCP server into each. With no `--agent`, every detected
agent is integrated; naming one integrates it even if discovery missed it.
`--bin` defaults to the running executable; `--db` (when given) is embedded in
the registered command and the Claude hook.

Per-agent targets and formats:

| Agent | File (user scope) | MCP key | Shell hook |
| --- | --- | --- | --- |
| Claude Code | `~/.claude.json` (project: `.mcp.json`) | `mcpServers` (stdio) | `SessionEnd` → `dream` in `~/.claude/settings.json` |
| OpenCode | `~/.config/opencode/opencode.json` | `mcp` (`type:"local"`, `command` array) | `plugins/memoryd.js` → `dream` on `session.idle` |
| Codex | `~/.codex/config.toml` | `[mcp_servers.memoryd]` | `[[hooks.Stop]]` → `dream` on turn stop |
| Hermes | `~/.hermes/config.yaml` | `mcp_servers` | `hooks.on_session_end` → `dream` |

Safety model: JSON files are parsed and deep-merged (other servers/settings
preserved, idempotent on re-run); TOML/YAML files are appended only when
memoryd's section is absent, a no-op when already present, and otherwise the
exact stanza is printed for manual paste. A present-but-unparseable JSON config
is an error, never overwritten. Every modified file is backed up to
`<file>.memoryd.bak`; `--dry-run` previews without writing.
