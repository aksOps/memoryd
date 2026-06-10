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

## A — Secondary-brain slice (planned, not implemented)

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
- `[ ]` A4. (Post-A3, optional) Multi-user/consent model for personas served
  to people other than the owner — trust boundary and disclosure decisions
  before any sharing feature.

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
- `[ ]` B3. Audit-log retention: an explicit policy decision — the table is
  append-only by trigger *by design*, so pruning needs a deliberate
  migration + documented integrity trade-off. Decide; do not silently relax.
- `[ ]` B4. `doctor --fix` file hygiene: `PRAGMA optimize`,
  `wal_checkpoint`, and a `VACUUM INTO`-based backup subcommand (also the
  documented safe-copy path while the daemon runs).
- `[ ]` B5. Persistent full-corpus HNSW (M9 follow-up): only needed if/when
  pure semantic search without a lexical prefilter over ~1M vectors becomes
  a requirement; the `VectorIndex` trait is the seam.
- `[ ]` B6. (Optional, far) Hierarchical re-consolidation: a new governed
  dream phase that compresses old memories into themes ("last year →
  principles"), with its own frontier marker. Same leash pattern; needs its
  own plan before any code.

## C — Provider completion (rest of the deferred M3 increment)

- `[x]` C1. Adapter failover: `reachable()` exists but nothing consumes it —
  worker/dream should probe before a batch and degrade to `local` for the
  pass (with an audit row) instead of burning retries when the endpoint is
  down.
- `[x]` C2. Runtime spend ledger: per-window (daily/weekly) spend
  enforcement across passes from `provider_usage`, not just the per-pass
  budget check.
- `[ ]` C3. `memoryd setup` CLI: interactive first-run config (adapter, base
  URL, key file, spend cap) writing env-file/instructions — the usability
  half of "opt-in providers".

## D — Architecture follow-ups (from the actor/MCP work)

- `[ ]` D1. Split inference out of `consolidate_pending` so the dream loop
  can join the single-writer actor (the documented `[~]` carve-out: today
  inference inside Store methods would serialize captures behind dream
  passes).
- `[x]` D2. MCP `memory_remember` provenance: `remember_event` hardcodes
  `agent: "cli"`; stamp `mcp` (and thread the real agent name through) so
  provenance distinguishes capture surfaces.
- `[x]` D3. Session-idle awareness (shipped inside A1's close-sweep) for `serve`: sessions never transition
  from `open` today except A1's distillation; consider a cheap
  `status='closed'` sweep in the dream pass so `sessions` reflects reality
  even without A1.

## E — Ops, docs, and capture practice (from the dev-knowledge discussion)

- `[ ]` E1. Capture-discipline guide: a short docs page on kinds
  (`decision`/`rule`/`observation`), tags, session-id hygiene, and the
  "capture decisions explicitly as one event" pattern — quality in determines
  persona quality out.
- `[ ]` E2. Backup/portability runbook: clean-shutdown single-file copy,
  `VACUUM INTO` for live copy, WAL sidecar caveat, adapter/model
  compatibility note for embeddings on the target machine (folds into B4).
- `[ ]` E3. Watchlist (no action): Turso `limbo` (pure-Rust SQLite,
  same file format) as a possible future drop-in behind the `Store` seam if
  it matures with FTS5; revisit yearly.

## F — Pre-existing milestone leftovers (unchanged by this session)

- `[ ]` F1. M10: benchmarks, packaging, prebuilt binaries / npm
  distribution.
- `[ ]` F2. M0: `doctor` disk-free evidence; release-build evidence
  decision; documented negative-test strategy for the SBOM/CVE gates.

## Suggested order

A1 → A2 → A3 (the committed plan, in increment order), then B1+B2 as one
retention increment, C1 (small, immediate reliability win — can ride along
with any A increment), then C2/C3, D1–D3 opportunistically, E1/E2 alongside
whichever increment touches the same docs. B3 needs an owner decision before
any code. B5/B6/A4/E3/F are demand-driven.
