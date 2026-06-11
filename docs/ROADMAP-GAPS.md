# Roadmap Gaps — Consolidated Task List (2026-06 session)

Everything actionable that came out of the 2026-06 working session: the
security review, the MCP/adapter implementation work, and the
secondary-brain / growth / dream-behavior discussions. Companion documents:
`SECURITY-REVIEW-TASKS.md` (all shipped), `SECONDARY-BRAIN-PLAN.md`
(increment-level detail for section A), `MILESTONE-TASKS.md` (milestone view).

Status legend: `[x]` shipped · `[~]` partial · `[ ]` open.

## Shipped this session (context, not work)

- `[x]` Security hardening P0–P3: socket timeouts, thread-per-connection with
  cap, per-IP auth throttle, token validation, redaction defense-in-depth,
  `--token-file`, protocol strictness (chunked/`ts_ms`/duration bounds),
  graceful shutdown, `GET /v1/health`, structured logging, `total_cmp`,
  `table_stats` pin, single-writer `store::Writer` actor.
- `[x]` MCP stdio facade: `memory_remember` / `memory_recall` /
  `memory_stats` / `memory_graph` + `Store::memory_neighbors`.
- `[x]` Generic `openai_compat` provider adapter (any OpenAI-shaped base URL,
  rustls TLS, env-configured, spend-cap-gated); provider-specific
  `ollama`/`opencode` names removed.

## A — Secondary-brain slice (shipped)

Detail in `SECONDARY-BRAIN-PLAN.md`; constraints: no new tables/deps, all
work in the dream plane, per-pass caps, H6 approvals gate.

- `[x]` A1. Session distillation dream phase: one synthesized
  `session_summary` memory per idle session (≤3 sessions/pass), linked over
  `memory_links`, `sessions.status` `open→distilled`, injection-hygienic
  prompt, re-redacted output.
- `[x]` A2. Heuristic extraction stage in extract-profile: induce ≤3
  field-agnostic decision principles per pass from recent decisions +
  session summaries; propose as `heuristic.*` facts into `approvals`
  (never direct writes).
- `[x]` A3. Persona surface: `memory_profile` MCP tool (approved facts +
  top-centrality themes) and the deferred MCP resources
  (`memory://profile`, `memory://session/{id}`) with `resources` capability.
- `[x]` A4. Decision recorded (2026-06): persona sharing to third parties is
  out of scope until the owner explicitly requests it. The MCP facade stays
  parent-process-trusted and single-owner; any sharing feature requires its
  own consent/disclosure design first. No code.

## B — Growth and sustainability (from the 2–3 year projection)

Growth today ≈ 3–5KB per capture, forever (~0.4GB/yr moderate, ~2GB/yr heavy);
only weak graph links are ever deleted. Recall stays fast by construction
(indexed FTS prefilter, 256-candidate cap), so these are about disk and
corpus hygiene, not query latency.

- `[x]` B1. Governed retention dream phase: archive/delete raw events older
  than N months **that are already consolidated**, keeping memories,
  provenance summaries, and the graph. Frontier-marked and per-pass capped
  like every other phase. Biggest lever (~80% of growth).
- `[x]` B2. Drop raw-event embeddings (via MEMORYD_RETAIN_RAW_EMBED_DAYS) once the corresponding memory-level
  embedding exists (second-largest growth component).
- `[x]` B3. Decision recorded (2026-06): the audit log remains append-only
  and unpruned — integrity outranks the ~0.2GB/yr worst-case growth. The
  B1 retention phase deliberately does not touch it. Revisit only if audit
  size becomes a measured problem; any future pruning needs a migration +
  documented integrity trade-off.
- `[x]` B4. `doctor --fix` file hygiene: `PRAGMA optimize`,
  `wal_checkpoint`, and a `VACUUM INTO`-based backup subcommand (also the
  documented safe-copy path while the daemon runs).
- `[x]` B5. Decision recorded (2026-06): demand-driven, intentionally not
  built. The shortlist-bounded brute-force path is sub-100ms at the design
  scale (A3); the `VectorIndex` trait remains the drop-in seam when pure
  full-corpus semantic search becomes a real requirement.
- `[x]` B6. Decision recorded (2026-06): deferred until session summaries
  and heuristics (A1/A2) have accumulated months of corpus to compress —
  building it now would have nothing to re-consolidate. Needs its own plan
  doc before code; same leash pattern as every dream phase.

## C — Provider completion (rest of the deferred M3 increment)

- `[x]` C1. Adapter failover: `reachable()` exists but nothing consumes it —
  worker/dream should probe before a batch and degrade to `local` for the
  pass (with an audit row) instead of burning retries when the endpoint is
  down.
- `[x]` C2. Runtime spend ledger: per-window (daily/weekly) spend
  enforcement across passes from `provider_usage`, not just the per-pass
  budget check.
- `[x]` C3. `memoryd setup` CLI: interactive first-run config (adapter, base
  URL, key file, spend cap) writing env-file/instructions — the usability
  half of "opt-in providers".

## D — Architecture follow-ups (from the actor/MCP work)

- `[x]` D1. Split inference out of the dream phases' write transactions:
  consolidate, distill, extract-profile, and heuristic induction all run
  provider calls in a compute phase BEFORE their (now millisecond-short)
  IMMEDIATE write transactions; associate already followed the pattern.
  The dream loop stays on its own connection — with no inference inside any
  write tx, WAL + busy_timeout bound capture impact to milliseconds, which
  resolves the carve-out's stated reason; routing dream writes through the
  actor remains an optional optimization with no remaining correctness
  motivation.
- `[x]` D2. MCP `memory_remember` provenance: `remember_event` hardcodes
  `agent: "cli"`; stamp `mcp` (and thread the real agent name through) so
  provenance distinguishes capture surfaces.
- `[x]` D3. Session-idle awareness (shipped inside A1's close-sweep) for `serve`: sessions never transition
  from `open` today except A1's distillation; consider a cheap
  `status='closed'` sweep in the dream pass so `sessions` reflects reality
  even without A1.

## E — Ops, docs, and capture practice (from the dev-knowledge discussion)

- `[x]` E1. Capture-discipline guide shipped: `docs/CAPTURE-GUIDE.md`.
- `[x]` E2. Backup/portability runbook shipped: `docs/OPERATIONS.md`
  (cold/live copy, restore, hygiene, embeddings/model caveat).
- `[x]` E3. Watchlist recorded (no action by design): Turso `limbo` as a
  possible pure-Rust SQLite drop-in behind the `Store` seam; revisit yearly
  (next: 2027-06).

## F — Pre-existing milestone leftovers (unchanged by this session)

- `[~]` F1. M10 remains the release milestone. Status recorded (2026-06):
  latency evidence exists as ignored perf fixtures (capture p95 ≈ 2.2ms,
  HTTP p95 ≈ 0.77ms, MILESTONE-TASKS M1); packaging/prebuilt binaries/npm
  publication require release infrastructure outside this repo's CI and
  stay demand-driven.
- `[x]` F2. M0 closed out: `doctor` reports disk_free_mb (shipped with B4);
  release-build evidence decision and the negative-test strategy for the
  SBOM/CVE gates are recorded in `docs/OPERATIONS.md`.

## Suggested order

A1 → A2 → A3 (the committed plan, in increment order), then B1+B2 as one
retention increment, C1 (small, immediate reliability win — can ride along
with any A increment), then C2/C3, D1–D3 opportunistically, E1/E2 alongside
whichever increment touches the same docs. B3 needs an owner decision before
any code. B5/B6/A4/E3/F are demand-driven.
