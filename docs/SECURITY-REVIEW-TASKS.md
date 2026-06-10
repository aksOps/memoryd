# Security Review Task List (2026-06)

This is the operational checklist produced by the full-codebase security and
quality review on the `claude/code-review-security-9zia87` branch. It follows
the same conventions as `docs/MILESTONE-TASKS.md` and should be updated as
items ship. Line references are against the commit the review was run on
(`3519256`).

Status legend:

- `[x]` shipped and verified.
- `[~]` partially shipped; remaining checklist items are required before the
  task is complete.
- `[ ]` not started.

Review verdict: no critical vulnerability (RCE, auth bypass, SQL injection)
found. P0 items are the only findings remotely exploitable on a non-loopback
bind; everything else is hardening, defense-in-depth, or planned-work tracking.

## P0 — Remotely Relevant Hardening (fix before any non-loopback deployment)

- `[x]` Set socket read/write timeouts on accepted HTTP connections.
  Shipped: 10s read/write deadlines set immediately after accept
  (`HTTP_READ_TIMEOUT`/`HTTP_WRITE_TIMEOUT`); timeouts surface as 408.
- `[x]` Handle connections off the accept loop. Shipped: thread-per-connection
  with a per-connection store, bounded by `MAX_CONCURRENT_CONNECTIONS` (64,
  excess → 503) via an RAII connection counter (`serve_loop`).
- `[x]` Throttle failed bearer-token auth. Shipped: per-IP fixed window
  (`AuthThrottle`): 5 × 401 within 60s locks the peer for 60s (429 before any
  request byte is read); success clears; map bounded at 1024 entries.
- `[x]` Reject empty bearer tokens at startup. Shipped: `Config::validate`
  rejects empty/whitespace tokens on any bind (`EmptyBearerToken`) and tokens
  under 16 chars on non-loopback binds (`BearerTokenTooShort`,
  `MIN_BEARER_TOKEN_LEN`).

## P1 — Secret Handling and Redaction Defense-in-Depth

- `[ ]` Re-redact `fact_value` before it is persisted into
  `approvals`/`profile_facts` (`crates/memoryd-core/src/store.rs:1086-1103`).
  Capture and import redact at the boundary, but the profile-fact path derives
  from memory content without its own redaction pass; a single upstream miss
  would be re-persisted and audit-logged.
- `[ ]` Document that `MEMORYD_TOKEN` is preferred over `--token` (CLI args
  are world-readable via `/proc/<pid>/cmdline`; environment is owner-only),
  and/or add `--token-file <path>`.
- `[ ]` Redactor: catch long digit-only secrets (16+ digits — PANs, numeric
  tokens). The high-entropy detector requires an alphabetic byte
  (`crates/memoryd-core/src/store.rs:2405`), so all-digit secrets escape.
- `[ ]` Redactor: make known API-key prefix matching case-insensitive
  (`crates/memoryd-core/src/store.rs:2354`).
- `[ ]` Redactor: decide whether URL-encoded secrets (split at `%xx`
  boundaries) are in scope; document the limitation either way.

## P2 — HTTP Protocol and Input-Handling Correctness

- `[ ]` Reject `Transfer-Encoding: chunked` explicitly (`501` or `411`).
  Today a chunked request parses as a zero-length body and returns a
  confusing JSON error (`crates/memoryd/src/main.rs:1048-1115`).
- `[ ]` Make `ts_ms` validation consistent in `capture_event_from_json`
  (`crates/memoryd/src/main.rs:1175-1180`): a string value is silently
  replaced with "now" while other non-number types are rejected; either
  reject strings too or parse them.
- `[ ]` Bound large duration/visibility config values instead of saturating.
  Huge `max_seconds` (`crates/memoryd-core/src/dream.rs:288`) or
  `lease_visibility_secs` (`crates/memoryd-core/src/worker.rs:28`) saturate to
  `i64::MAX`, silently meaning "no cap"; validation should reject (e.g.) >24h.

## P3 — Robustness and Code-Quality Improvements

- `[ ]` Graceful shutdown: handle SIGTERM, drain the embed worker and dream
  scheduler (both are detached threads today), close the listener. Currently
  relies entirely on SQLite crash safety (noted as deferred in
  `crates/memoryd/src/main.rs:285-287`).
- `[ ]` Consolidate the three writers (HTTP handler, embed worker, dream
  loop) onto the planned single-writer `store::Writer` actor
  (ARCHITECTURE-PLAN §7.1/U5). WAL + `busy_timeout=5000` makes today's shape
  safe but contention-prone.
- `[ ]` Add a `GET /v1/health` endpoint (doctor/stats exist only as CLI);
  useful for supervisors and the planned npm distribution.
- `[ ]` Pin `table_stats` to the const table list defensively
  (`crates/memoryd-core/src/store.rs:1316`): the `format!`-built `SELECT
  COUNT(*) FROM {table}` is safe today because it iterates hardcoded
  `CANONICAL_TABLES`, but it is the one interpolated SQL site — add a comment
  or debug assertion so a refactor can't make it dynamic.
- `[ ]` Use `total_cmp` in the brute-force vector sort for consistency with
  HNSW (`crates/memoryd-core/src/vectorindex.rs:40-44`); harmless today since
  vectors are L2-normalized, but future-proof.
- `[ ]` Adopt structured logging (`log`/`tracing`) in place of
  `println!`/`eprintln!` before the worker count grows further.

## Verified Non-Issues (do not re-open without new evidence)

- `[x]` SQL injection: all user-facing queries use bound parameters; FTS
  MATCH tokens are filtered to alphanumeric/underscore before quoting.
- `[x]` Queue-depth TOCTOU: the check runs inside the same write transaction
  that already holds SQLite's writer lock (`store.rs:123-171`), so the limit
  cannot be exceeded by interleaving writers.
- `[x]` Capture/import redaction coverage: payload, provenance, metadata
  fields, FTS content, and dedup hashes are all computed over redacted text;
  tests assert secrets do not survive into recall results.
- `[x]` Auth ordering: authorization is checked before routing, so
  unauthorized callers cannot enumerate routes; peer address comes from the
  socket, not headers.
- `[x]` Supply chain: GitHub Actions SHA-pinned with read-only permissions;
  embed model and security tools are SHA-256-pinned; advisory ignores
  (`paste`, compile-time-only) are justified and documented.
