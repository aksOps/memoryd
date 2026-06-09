# memoryd API

This document describes the current external interfaces. The implementation is
early and intentionally narrow.

## CLI

### `doctor`

```bash
memoryd doctor [--db <path>] [--bind <addr:port>] [--token <token>]
```

Checks SQLite schema, WAL mode, foreign keys, and configuration safety.

### `stats`

```bash
memoryd stats [--db <path>] [--bind <addr:port>] [--token <token>]
```

Prints row counts for canonical tables.

### `remember`

```bash
memoryd remember <content> [--kind <kind>] [--session <id>] [--source <source>] [--tags <a,b>] [--db <path>]
```

Writes a memory capture through the same append-only path as HTTP capture. It
redacts common secret shapes before persistence, returns after the raw event and
one embed job are persisted, writes capture/redaction audit rows, and does not
call a provider inline.

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
memoryd serve [--db <path>] [--bind <addr:port>] [--token <token>]
```

Starts the local HTTP server. The default bind is `127.0.0.1:7077`. Any
non-loopback bind requires a bearer token at startup.

## REST

Base URL: `http://127.0.0.1:7077` by default.

### `POST /v1/capture`

Redacts common secret shapes, appends the redacted raw event, upserts its
session, enqueues one `embed` job, writes capture/redaction audit rows, and
returns immediately. The handler performs no provider calls.

Request headers:

```http
Content-Type: application/json
Authorization: Bearer <token>
```

`Authorization` is required when a token is configured. Loopback calls may omit
authorization when no token is configured.

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

Current status codes: `400`, `401`, `404`, `405`, `413`, `415`, `422`, `431`,
and `500`.

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
