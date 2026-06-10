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

- `[x]` Re-redact `fact_value` before it is persisted into
  `approvals`/`profile_facts`. Shipped: redaction at both persistence points —
  `extract_profile_pending` (before `proposed_change`/audit rows) and
  `decide_approval` (before the `profile_facts` INSERT, with a redaction-count
  audit detail).
- `[x]` Token-handling guidance. Shipped: `--token-file <path>` flag (trailing
  newline trimmed, `chmod 0600`-able), plus README/help docs preferring
  `MEMORYD_TOKEN`/`--token-file` over `--token` (argv is world-readable via
  `/proc/<pid>/cmdline`).
- `[x]` Redactor: long digit-only secrets. Shipped: all-digit runs of 16+
  digits are redacted (`DIGIT_SECRET_MIN_LEN`); 13-digit unix-ms timestamps
  stay untouched.
- `[x]` Redactor: known API-key prefix matching is now case-insensitive via
  the existing `find_ascii_case_insensitive` helper (zero allocation).
- `[x]` Redactor: URL-encoded secrets — decision recorded: out of scope.
  Documented as a known limitation at `redact_inline_string_with_count` and in
  the README Security Defaults section (decoding arbitrary text risks false
  positives and double-decode bugs for marginal gain on a local-first daemon).

## P2 — HTTP Protocol and Input-Handling Correctness

- `[x]` Reject `Transfer-Encoding: chunked` explicitly. Shipped: requests
  carrying a chunked transfer-encoding header get `501 not_implemented`
  ("send Content-Length") right after auth, before any body parsing.
- `[x]` Make `ts_ms` validation consistent. Shipped: the silent string→now
  arm is gone; any non-integer `ts_ms`/`ts` (including numeric strings) is
  rejected with 422 "ts_ms must be integer milliseconds".
- `[x]` Bound large duration/visibility config values. Shipped:
  `Config::validate` rejects `dream_wallclock_secs`/`lease_visibility_secs`
  over `MAX_DURATION_SECS` (24h), and `dream --max-seconds` enforces the same
  bound at parse; the saturating conversions remain as commented backstops.

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
