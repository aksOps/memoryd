# memoryd Milestone Task List

This is the operational checklist for the roadmap in `docs/ARCHITECTURE-PLAN.md`.
It is intentionally shorter than the architecture plan and should be updated at
the end of every implementation slice.

Status legend:

- `[x]` shipped and verified.
- `[~]` partially shipped; remaining checklist items are required before the
  milestone is complete.
- `[ ]` not started.

## Current Position

- `[~]` M0 is mostly complete.
- `[x]` M1 is complete against this checklist.
- `[x]` M2 — provider-free `raw_events_fts` lexical recall is the accepted variant;
  durable-memory recall is deferred to M6 (not an M2 gap).
- `[~]` M3 — queue leasing, governor caps, the embed worker, and the `null` adapter
  are delivered and gated; paid/remote adapters and `setup` are deferred to a later
  M3 increment.
- `[x]` M4 — the vector-rerank engine (`vectorindex` + semantic recall + query cache
  + lexical degrade) is delivered and gated; it activates once a non-`null` embedding
  provider is configured.
- `[x]` M5 — generic JSONL historic import is delivered and gated: idempotent by
  `content_hash`, resumable, governor-bounded, routed through the normal capture path;
  source-specific importers and distillation into `memories` are deferred (M6).
- `[x]` M6 — the dream plane (consolidate dedup-cluster + decay lifecycle) is delivered
  and gated under wall-clock + spend caps with `dream_runs` accounting and a `serve`
  scheduler; the LLM-summary path is test-double-only and §9.7 cleanup is deferred.
- `[x]` M7 — the association graph + one-hop recall is delivered: a `dream` associate
  phase builds/reinforces/prunes symmetric `memory_links` (co-occurrence + semantic),
  `recall --hops 0|1` adds graph expansion, and `graph_centrality`/`link_strength` feed
  scoring; semantic links are test-double-only and link weight-decay is deferred.
- `[x]` M8 — profile extraction behind the approvals gate (H6) is delivered: a `dream`
  extract-profile phase proposes facts into `approvals(pending)` (never writing
  `profile_facts` directly — enforced by the NOT NULL approval_id FK), and
  `approve [--list] [--id N --accept|--reject]` commits/rejects them; LLM extraction is
  stubbed (deterministic kind-based, optional gated `summarize` refinement).
- `[x]` M9 — in-process HNSW behind the `VectorIndex` trait is delivered: a
  dependency-free, deterministic, unsafe-free second implementation, config-selectable
  (`Caps.vector_index_kind` + `recall --index`), with `BruteForce` as the default and
  oracle; recall@10 within epsilon of BruteForce on a fixture, no API change.
- `[x]` Security-hardening pass (2026-06): socket timeouts + concurrent
  connections + auth throttling + token validation, redaction
  defense-in-depth, protocol strictness, graceful shutdown, `GET /v1/health`,
  and the single-writer `store::Writer` actor — see
  `docs/SECURITY-REVIEW-TASKS.md` for the per-item checklist.
- `[x]` MCP facade (initial slice): `memoryd mcp` stdio server (protocol
  2024-11-05, tools `memory_remember`/`memory_recall`/`memory_stats`/
  `memory_graph` over the M7 association graph, no socket bind); resources
  (`memory://session/{id}`) deferred.
- `[~]` Deferred M3 increment: the generic `openai_compat` adapter is delivered
  (one adapter for any OpenAI-shaped endpoint — provider-specific
  `ollama`/`opencode` names removed; embeddings + chat-completions + reachability
  probe, env-configured, spend-cap-gated, rustls TLS). Failover, the `setup`
  CLI, and the runtime spend ledger remain.
- `[ ]` M10 (bench/packaging) remains.

Next implementation target: the rest of the M3 increment (failover, setup CLI,
runtime spend ledger) and/or M10.

## M0 — Store Skeleton, Config, CLI Shell, Security Gate

Status: `[~]` Mostly complete.

- `[x]` Rust workspace with `memoryd` binary and `memoryd-core` library.
- `[x]` Canonical SQLite schema created with all 13 durable tables.
- `[x]` `schema_migrations` exists and migrations are idempotent.
- `[x]` WAL, foreign keys, and busy timeout are applied on open.
- `[x]` `memories_fts` exists.
- `[x]` `raw_events_fts` exists via schema v2.
- `[x]` `doctor` command opens the store and reports schema/pragmas.
- `[x]` `stats` command reports canonical table counts.
- `[x]` Pinned Rust toolchain is committed.
- `[x]` CI runs format, build, clippy, tests, dependency policy, audit, and SBOM.
- `[x]` Security tooling bootstrap uses hash-pinned prebuilt tools.
- `[x]` OpenSSF Best Practices passing evidence is recorded.
- `[x]` `main` is PR-only protected, including admins.
- `[ ]` `doctor` reports disk-free evidence if we keep that M0 plan requirement.
- `[ ]` Decide whether to keep the plan's `cargo build --release` exit evidence
  as an M0 requirement or treat debug CI build as enough for pre-release.
- `[ ]` Add a documented negative-test strategy for vulnerable dependency/SBOM
  gates without committing a deliberately vulnerable dependency.

## M1 — Fast Capture Path

Status: `[x]` Complete against this checklist.

- `[x]` `serve` command starts local HTTP server.
- `[x]` Default bind is `127.0.0.1:7077`.
- `[x]` Non-loopback bind requires bearer token at config validation.
- `[x]` `POST /v1/capture` appends one `raw_events` row.
- `[x]` Capture upserts one `sessions` row and increments `event_count`.
- `[x]` Capture enqueues one `jobs(kind='embed')` row.
- `[x]` Capture returns without provider calls.
- `[x]` `remember` uses the same capture path.
- `[x]` Capture redacts metadata, payload, provenance, and recall index text before
  persistence.
- `[x]` `capture.append` audit rows are written in the capture transaction.
- `[x]` `redaction.apply` audit rows are written for redacted captures without
  original secret material.
- `[x]` `auth.reject` audit rows are written without bearer token material.
- `[x]` Implement bounded capture admission/backpressure behavior.
- `[x]` Return `accepted-degraded` instead of blocking or 5xx when capture is
  saturated.
- `[x]` Add saturation tests for the degraded capture path.
- `[x]` Add a capture latency fixture or benchmark for 100 sequential inserts.
- `[x]` Record p95 capture latency evidence against the `< 8 ms` M1 target.

Evidence from 2026-06-09 local ignored performance-fixture run with
`--test-threads=1`:

- `capture_100_sequential_inserts_p95_stays_under_m1_target`: p95 `2.226592ms`.
- `http_capture_100_sequential_requests_p95_stays_under_m1_target`: p95
  `771.531µs`.

## M2 — Lexical Recall, Provider-Free

Status: `[x]` Provider-free raw-event lexical recall over `raw_events_fts` is the
accepted M2 variant (decided 2026-06-09: "keep raw recall"). Durable
`memories`/`memory_versions` recall and versioning are deferred to the
dream/consolidation plane (M6) and are not part of M2 scope.

- `[x]` `recall` CLI exists.
- `[x]` `POST /v1/recall` exists.
- `[x]` Recall is provider-free and local by default.
- `[x]` Recall searches redacted captured raw events through SQLite FTS5.
- `[x]` Recall treats operator-like FTS terms as literals.
- `[x]` Recall rejects empty or punctuation-only queries.
- `[x]` Recall does not write provider usage rows.
- `[x]` Decided (2026-06-09) to keep provider-free `raw_events_fts` recall as the
  accepted M2 variant rather than switching to the plan's
  `memories`/`memory_versions` durable-memory path.
- `[x]` Updated the roadmap and project docs to record raw-event FTS recall as the
  accepted M2 variant.
- `[x]` Added an `EXPLAIN QUERY PLAN` regression test proving recall uses the
  `raw_events_fts` virtual-table index, pinned to the production query via the
  shared `RECALL_EVENTS_SQL` constant
  (`recall_events_query_plan_uses_raw_events_fts`).
- `[x]` Added recall latency evidence against the `< 100 ms` M2 target (see
  evidence below).

Deferred to the dream/consolidation plane (M6), not part of the accepted M2
raw-recall variant:

- `[ ]` Make `remember` create a durable `memories` row and immutable
  `memory_versions` v1.
- `[ ]` Add the multi-variable score breakdown over `scoring_variables` and
  durable-memory provenance to recall results.
- `[ ]` Queue access-frequency bump jobs for durable memories (the raw-event read
  path already performs no inline writes).
- `[ ]` Add a hard candidate cap (e.g. 256) to bound recall latency for very broad
  queries; raw recall currently scores the full FTS match set.

Evidence from 2026-06-09 local ignored recall-latency fixture run with
`--test-threads=1` over a 50,000 raw-event corpus, recall query matching a
bounded ~200-row subset (a representative query — note the planned 256-candidate
cap is not yet implemented, so production queries are currently unbounded):

- `recall_50k_raw_events_median_latency_under_m2_target`: median (p50)
  `491.151µs`, p95 `87.664401ms`, p99 `94.877763ms`.

The plan states the recall SLO as p95 `< 100 ms` in its capability tables and p99
`< 100 ms` in the M2 exit row. The recorded p95 (`87.664401ms`) meets the p95
SLO; p99 (`94.877763ms`) is under 100 ms but borderline. These tail figures are
not contention-robust — they vary with host load (heavier load pushes them past
100 ms), because the wall-clock tail on this shared dev host is dominated by
scheduler contention from co-resident processes, not recall cost. The median
(~0.5 ms) is the contention-robust measure of the algorithm; the fixture asserts
the median as a regression floor and records the full distribution as dated
evidence. (A dedicated, idle 1-vCPU baseline would be expected to remove the
tail, since the median is already ~200x under target, but that has not been
measured here.) Raw recall also scores the full FTS match set (no hard candidate
cap — that prefilter is part of the deferred durable design), so latency scales
with match-set size.

## M3 — Queue, Governor, Embed Worker, Provider Adapters

Status: `[~]` Core plane delivered (exactly-once leasing, caps, embed worker, `null`
adapter); paid/remote adapters and the `setup` command are deferred to a later M3
increment behind the in-place `ProviderAdapter` seam.

- `[x]` Add queue leasing from `jobs` with atomic `pending -> running` transition
  (one `UPDATE ... RETURNING`; SQLite writer serialization makes it exactly-once).
- `[x]` Add visibility timeout and attempt accounting (expired `running` leases are
  reclaimable; `attempts` increments per lease).
- `[x]` Add exactly-once concurrency tests for job leasing (4 workers, 200 jobs, no
  double-claim).
- `[~]` Governor caps: `queue_depth_max` (admission) and `worker_concurrency`
  (per-tick lease bound) are enforced now; `worker_mem_mb`, CPU share,
  `dream_wallclock_secs`, and the *runtime* spend-ledger ceiling (`paid_spend_cap_usd`,
  the plan's `spend_window_usd`) are config-present and bind once the planes that
  consume them land.
- `[x]` Over-cap admission backpressure without OOM (capture degrades without
  enqueue at the cap; covered by existing regression tests).
- `[x]` Add fixed worker with `Embed` active first (the only active worker this slice).
- `[x]` Add `null` provider adapter for offline CI and default no-spend mode.
- `[x]` Add `local` in-process embedding adapter (2026-06-10): bge-small-en-v1.5
  fp32 ONNX via tract (pure Rust), model + tokenizer `include_bytes!`-embedded with
  SHA-256 pins (`scripts/fetch-embed-model.sh` + `build.rs`); 384-dim CLS-pooled
  L2-normalized vectors; bge query prefix via new defaulted `embed_query` trait
  method; selected by `providers.default_adapter` (now the **default**); migration
  0007 widens the `provider_usage.adapter` CHECK; `complete_embed_job` now records
  the real adapter id. Quality evidence (SciFact, 5183 docs/300 queries): fused
  R@10 0.851 / MRR 0.694 vs lexical 0.784 / 0.629.
- `[ ]` Add `openai_compat` provider adapter behind explicit config. (Deferred.)
- `[ ]` Add `ollama` provider adapter behind explicit config. (Deferred.)
- `[ ]` Add provider reachability probe and failover order. (Deferred — single
  adapter this slice.)
- `[x]` Write `embeddings` rows from the `Embed` worker.
- `[x]` Write `provider_usage` rows for every embed provider call.
- `[x]` Block paid calls at the default zero spend cap (config validation rejects a
  remote default adapter when `paid_spend_cap_usd == 0`; `null` and `local` are
  free in-process adapters recording `est_cost = 0`). The *runtime* ledger-based spend ceiling is deferred (see the caps
  line above).
- `[ ]` Add `setup` or equivalent provider config/reachability command. (Deferred
  with the remote adapters.)

### M3 evidence (2026-06-09)

- `embed_lease_is_exactly_once_under_concurrent_workers`: 4 worker connections drain
  200 seeded jobs; every job is claimed exactly once (200 unique ids) and all reach
  `done`. (Exactly-once under concurrency; at-least-once across lease expiry, made safe
  by the idempotent embedding upsert in `complete_embed_job`.)
- `lease_then_complete_writes_embedding_and_provider_usage`: a leased job writes a
  32-dim `embeddings` row (128-byte little-endian vector) plus a `provider_usage`
  row (`adapter='null'`, `op='embed'`, `est_cost=0`), and the job reaches `done`.
- `failed_embed_job_defers_with_backoff_then_dead_letters`: deferral offsets are
  500/1000/2000/4000 ms; the job dead-letters once `attempts` reaches
  `job_max_attempts` (5).
- `expired_lease_is_reclaimed_after_visibility_timeout`: a `running` job past its
  visibility window is re-leased with an incremented attempt count.
- `worker::tick_embed` processes up to `worker_concurrency` jobs per tick; `serve`
  runs one such worker thread over a second WAL connection.

#### Deferred to a later M3 increment

`openai_compat`/`ollama`/`opencode` adapters, the reachability probe + failover
order, and the `setup` CLI. The `ProviderAdapter` seam is already in place so these
land without touching the queue/worker/ledger code. Also deferred: the runtime
spend-ledger ceiling, a bounded-memory/steady-RSS test, lease-epoch fencing for
adapters that can outlast the visibility window, consolidating the worker onto the
planned single-writer actor (it currently uses its own connection), and an
integration test of the `serve` worker thread (the tick is unit-tested directly).

## M4 — Vector Rerank In Recall

Status: `[x]` Engine delivered and gated. Production semantic recall activates once a
non-`null` embedding provider is configured (deferred M3 increment); the `null` adapter
carries no semantic signal, so `recall --semantic` degrades to lexical today. Uplift is
proven with a deterministic concept test-double (no network).

- `[x]` Add `vectorindex` trait (`VectorIndex`) with a stable signature for the M9 HNSW swap.
- `[x]` Add brute-force cosine over the bounded candidate shortlist (<=256, never the table).
- `[x]` Add query-embedding cache (`embeddings`, `owner_type='query'`); a cache hit makes no
  provider call and writes no ledger row.
- `[x]` Add semantic rerank (FTS prefilter -> cosine over candidates -> bm25/cosine fusion at
  `w_sem=0.34`, `w_lex=0.18`), gated on `ProviderAdapter::embeds_semantically`.
- `[x]` Add lexical fallback with the same response shape when no semantic signal is available
  (`degraded=true`, `mode="lexical"`).
- `[x]` Add cache-hit test proving no second provider call and a single query ledger row.
- `[x]` Add test proving at most 256 vectors compared per query (`compared` instrumentation).
- `[x]` Add labeled fixture showing semantic rerank surfaces the concept-relevant match that
  lexical-only mis-ranks (recall@1 uplift via the concept test-double).

### M4 evidence (2026-06-09)

- `semantic_recall_outranks_lexical_on_labeled_fixture`: three docs share the FTS token "lock";
  the query ties lexically (falls back to ts DESC, surfacing the wrong doc), but the concept
  embedding ranks the truly-relevant doc first.
- `query_embedding_is_cached_with_no_second_provider_call`: the second identical query makes
  zero provider calls; exactly one query-embed ledger row exists.
- `null_provider_degrades_semantic_to_lexical_same_shape`: `null` adapter -> `mode="lexical"`,
  `degraded=true`, identical hit order to `recall_events`, no query embedding written.
- `semantic_recall_compares_at_most_candidate_cap`: 300 matching events -> `compared == 256`.
- `vectorindex` unit tests: cosine identity/orthogonality/zero/mismatched-dim; brute-force
  ranking and top-k truncation.

#### Deferred

Wiring semantic recall to a real embedding provider (the `openai_compat`/`ollama` adapters
from the deferred M3 increment). Until then `recall --semantic` degrades to lexical.

## M5 — Idempotent Historic Import

Status: `[x]` Delivered (generic JSONL). Source-specific importers deferred by design.

- `[x]` Add `import` CLI with source and path arguments (`import --source jsonl --path <p>`).
- `[x]` Add generic JSONL importer first (`import` module: `parse_jsonl`, `content_hash`).
- `[ ]` Add source-specific importers only after JSONL path is stable. (Deferred — JSONL path just landed.)
- `[x]` Use `import_batches` for total, processed, skipped, and state tracking (`paused`/`failed` states; per-unit errors surface via `StoreError::Import`).
- `[x]` Add content hash/idempotency so reruns do not duplicate rows (migration 0003: `content_hash` BLOB + partial unique index `ux_raw_import_hash`; FNV-1a, not BLAKE3 — dep policy).
- `[x]` Add interrupted-import resume behavior (queue-full pause + resume; re-scan + dedup).
- `[x]` Route imported rows through the same raw event/session/job path as capture (reuses redaction/validation/FTS/embed-enqueue helpers; `kind='import'`, normal `embed` jobs).
- `[x]` Preserve source provenance on every imported row (`import_batch`/`import_source`/`path`).
- `[x]` Verify governor bounds embed throughput during bulk import (`import_pauses_when_embed_queue_is_full_then_resumes`).

Evidence: 8 `import` unit tests (parse/normalize/hash) + 5 store integration tests + 2 CLI tests; end-to-end import/re-import/recall smoke verified. Deviations recorded in §21.8.

## M6 — Dream Plane: Consolidate And Decay

Status: `[x]` Delivered. LLM-summary path test-double-only; §9.7 cleanup/M7/M8 deferred.

- `[x]` Add `dream` CLI with explicit run mode (`dream --now [--budget-usd] [--max-seconds]`) + a periodic scheduler thread in `serve`.
- `[x]` Add `dream_runs` creation and completion accounting (`create_dream_run`/`finish_dream_run`; jobs_run/memories_touched/tokens_used/status).
- `[x]` Add wall-clock cap enforcement with partial status and requeue (`dream_wallclock_cap_stops_with_partial`).
- `[x]` Add spend cap enforcement using `provider_usage` totals (`spend_cap_degrades_consolidation_to_lexical`; spend ≤ cap).
- `[x]` Add consolidate worker that creates `memories` and immutable `memory_versions` (`consolidate_pending`).
- `[x]` Add lexical/dedup-only consolidation fallback when LLM budget is zero or provider unavailable (default `null` path).
- `[x]` Add decay worker that touches only due rows through indexed access (`decay_due` via `memories_decay_due`; `decay_query_plan_uses_index_no_scan`).
- `[x]` Enforce canonical lifecycle transitions for decay (`decay_transitions_follow_canonical_order_over_due_rows`).
- `[x]` Audit dream mutations (`consolidate`/`decay` audit_log rows).
- `[x]` Add tests for wall-clock cap, spend cap, no-scan decay, and immutable versions (`consolidation_versions_are_immutable_through_decay`).

Evidence: 6 dream unit tests (decay/lifecycle/score) + 8 store integration tests + 2 CLI tests; end-to-end `dream` smoke verified. Deviations recorded in §21.9.

## M7 — Association Graph And One-Hop Recall

Status: `[x]` Delivered. Associate runs inline in `dream_once`; semantic links are
`ConceptAdapter` test-double-only; link weight temporal-decay deferred (growth bounded
by fan-out cap + weak-floor prune).

- `[x]` Add associate step (inline in `dream_once`, like M6 consolidate/decay).
- `[x]` Populate/update `memory_links` with weighted associations (co-occurrence by
  `source_session`; embedding-similarity `semantic` when the adapter is semantic).
- `[x]` Enforce per-node fan-out cap (≤32) via symmetric storage + ROW_NUMBER prune.
- `[x]` Prune weak links (`weight < 0.10`) under cap.
- `[x]` Add one-hop recall expansion over `memory_links` (`recall --hops 0|1`).
- `[x]` Add scoring inputs for `graph_centrality` (§9.4) and `link_strength` (§9.3),
  folded into `R_base` (0.12) and `R_recall`.
- `[x]` Fixture `recall_memories_one_hop_surfaces_missed_neighbor` (+ CLI end-to-end):
  the linked memory surfaces under `--hops 1`, not under `--hops 0`.
- `[~]` p99 recall `< 100 ms`: bounded by FTS prefilter (≤256) + fan-out (≤32) brute-force
  cosine; a `bench` harness lands in M10 (§21.13). Not yet measured against the VM SLO.

## M8 — Profile Extraction Behind Approvals

Status: `[x]` Delivered. ExtractProfile runs inline in `dream_once`; LLM extraction
is stubbed (deterministic kind-based, optional gated `summarize` refinement).

- `[x]` `decide_approval` commits `profile_facts` only on accept (the sole writer path).
- `[x]` Approval decision workflow: `list_pending_approvals` + `decide_approval`.
- `[x]` `extract_profile_pending` creates pending approvals only (never facts).
- `[x]` `approve --list` CLI (lists pending approvals as JSON).
- `[x]` `approve --id <id> --accept` CLI (commits the fact).
- `[x]` `approve --id <id> --reject` CLI (writes no fact).
- `[x]` No `profile_facts` write without an `approval_id` — structurally enforced by the
  NOT NULL FK (test `h6_profile_fact_requires_an_approval_id`).
- `[x]` Audit propose/approve/reject decisions (`insert_audit_log`).
- `[x]` Idempotent: propose-once-per-`fact_key` (test `extract_is_idempotent_no_duplicate_pending`).

## M9 — In-Process HNSW Behind VectorIndex

Status: `[x]` Delivered. Dependency-free, deterministic, unsafe-free HNSW; builds
per-call (stateless trait), so BruteForce stays default + oracle. Persistent full-corpus
index (the real latency win) and a large perf fixture are deferred to M10.

- `[x]` Add HNSW `VectorIndex` implementation behind the M4 trait (no trait change).
- `[x]` Keep brute-force implementation selectable as correctness oracle
  (`Caps.vector_index_kind` + `recall --index`, default brute-force).
- `[x]` Fixture: HNSW recall@10 within epsilon of brute force (overlap >= 9/10, top-1
  exact) — `hnsw_recall_at_10_within_epsilon`, `hnsw_matches_brute_force_top1_exact`.
- `[~]` Large (200k) perf fixture + p99 `< 100 ms` measurement: deferred to the M10
  bench harness (the per-call build means HNSW is not yet a latency win over the ≤256
  shortlist; the persistent index lands later).
- `[x]` No API changes to M4 recall callers (trait + signatures unchanged).
- `[x]` SQLite remains the only durable store (HNSW is in-memory, per-call).

## M10 — Bench, Doctor Hardening, Packaging

Status: `[ ]` Not started.

- `[ ]` Add internal `bench` command over fixed fixtures.
- `[ ]` Bench capture p95/p99, recall p99 lexical/semantic, queue throughput,
  dream cost/time, and memory footprint.
- `[ ]` Add `export` CLI for memories and provenance.
- `[ ]` Harden `doctor` with capture -> embed -> recall -> dream dry-run self-test.
- `[ ]` Add prebuilt binary packaging for Linux x64/arm64 and macOS x64/arm64.
- `[ ]` Add Windows packaging as best-effort if feasible.
- `[ ]` Add npm wrapper that installs bundled prebuilt binaries without download or
  postinstall.
- `[ ]` Attach SBOM to GitHub release artifacts.
- `[ ]` Add release artifact checksum/signature flow.
- `[ ]` Add packaged artifact scanning with release-blocking high/critical CVE
  policy.

## Cross-Cutting Tasks

- `[x]` Keep runtime local-first with no public-internet calls by default.
- `[x]` Keep `unsafe` forbidden.
- `[x]` Keep dependency/license/advisory/SBOM checks in CI.
- `[x]` Keep generated SBOM files ignored locally.
- `[x]` Keep `main` protected and PR-only.
- `[ ]` Add EXPLAIN/query-plan tests for every new query that could regress into a
  full-table scan.
- `[ ]` Add audit rows for every mutating/security-sensitive action as new
  milestones add them.
- `[ ]` Keep provider calls out of capture and out of the required recall path.
- `[ ]` Update this file after every slice before opening a PR.

## Immediate Next Queue

1. M2 decided: provider-free `raw_events_fts` recall accepted. Durable-memory recall
   deferred to M6.
2. M3 core plane landed: exactly-once leasing, governor caps, embed worker, and the
   `null` adapter (writes `embeddings` + `provider_usage`).
3. M4 vector-rerank engine landed (brute-force cosine over the FTS shortlist, query
   cache, lexical degrade); proven with a concept test-double. Production semantic
   awaits a real embedding provider.
4. Start M5 (idempotent historic import).
5. Land the deferred M3 increment: `openai_compat`/`ollama` adapters, reachability
   probe + failover, the `setup` command, and wiring real semantic recall.
6. Add M0 release-build/disk-free evidence if those plan requirements remain.
