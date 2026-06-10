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
  "schema_version": 2
}
```

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
- `memory_graph` — `{memory_id (required), limit 1–50 = 16}`. Direct
  association-graph neighbors of a memory over `memory_links`, strongest link
  first, each with `link_type` (`semantic`, `co_occurrence`, ...),
  `link_strength`, `last_reinforced_at`, and a content snippet (240 chars).

Example call:

```json
{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"memory_graph","arguments":{"memory_id":"27938c38d764..."}}}
```

### Error mapping

Protocol failures are JSON-RPC errors: `-32700` unparseable line, `-32600`
invalid request / oversized line, `-32601` unknown method, `-32602` unknown
tool or invalid tool arguments, `-32002` request before `initialized`,
`-32603` internal store error (generic message; detail goes to stderr only).

Execution failures are in-band tool results with `isError: true` so the
calling model can react: blank recall query ("query must contain searchable
text"), empty capture fields ("capture fields must not be empty"), and unknown
`memory_id` ("memory not found").
