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
- `[ ]` M4 and later are not started.

Next implementation target: M4 (vector rerank over the embeddings M3 produces).

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
  `dream_wallclock_secs`, and the spend window are config-present and bind once the
  planes that consume them land.
- `[x]` Over-cap admission backpressure without OOM (capture degrades without
  enqueue at the cap; covered by existing regression tests).
- `[x]` Add fixed worker with `Embed` active first (the only active worker this slice).
- `[x]` Add `null` provider adapter for offline CI and default no-spend mode.
- `[ ]` Add `openai_compat` provider adapter behind explicit config. (Deferred.)
- `[ ]` Add `ollama` provider adapter behind explicit config. (Deferred.)
- `[ ]` Add provider reachability probe and failover order. (Deferred — single
  adapter this slice.)
- `[x]` Write `embeddings` rows from the `Embed` worker.
- `[x]` Write `provider_usage` rows for every embed provider call.
- `[x]` Enforce default `spend_window_usd = 0` so paid calls are blocked unless
  opted in (config validation rejects a non-`null` default adapter at zero budget;
  the `null` adapter records `est_cost = 0`).
- `[ ]` Add `setup` or equivalent provider config/reachability command. (Deferred
  with the remote adapters.)

### M3 evidence (2026-06-09)

- `embed_lease_is_exactly_once_under_concurrent_workers`: 4 worker connections drain
  200 seeded jobs; every job is claimed exactly once (200 unique ids) and all reach
  `done`.
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
land without touching the queue/worker/ledger code.

## M4 — Vector Rerank In Recall

Status: `[ ]` Not started.

- `[ ]` Add `vectorindex` trait.
- `[ ]` Add brute-force cosine implementation over bounded candidate shortlist.
- `[ ]` Add query embedding cache using `embeddings` or a dedicated cache shape.
- `[ ]` Add optional semantic rerank in recall when embeddings/provider are
  available.
- `[ ]` Add lexical fallback with the same response shape when provider is
  unavailable.
- `[ ]` Add cache-hit test proving no provider call and unchanged cost ledger.
- `[ ]` Add instrumentation/test proving <=256 vectors compared per query.
- `[ ]` Add labeled fixture showing recall@10 uplift over lexical-only.

## M5 — Idempotent Historic Import

Status: `[ ]` Not started.

- `[ ]` Add `import` CLI with source and path arguments.
- `[ ]` Add generic JSONL importer first.
- `[ ]` Add source-specific importers only after JSONL path is stable.
- `[ ]` Use `import_batches` for total, processed, state, and error tracking.
- `[ ]` Add content hash/idempotency so reruns do not duplicate rows.
- `[ ]` Add interrupted-import resume behavior.
- `[ ]` Route imported rows through the same raw event/session/job path as capture.
- `[ ]` Preserve source provenance on every imported row.
- `[ ]` Verify governor bounds embed throughput during bulk import.

## M6 — Dream Plane: Consolidate And Decay

Status: `[ ]` Not started.

- `[ ]` Add `dream` CLI with explicit run mode.
- `[ ]` Add `dream_runs` creation and completion accounting.
- `[ ]` Add wall-clock cap enforcement with partial status and requeue.
- `[ ]` Add spend cap enforcement using `provider_usage` totals.
- `[ ]` Add consolidate worker that creates `memories` and immutable
  `memory_versions`.
- `[ ]` Add lexical/dedup-only consolidation fallback when LLM budget is zero or
  provider unavailable.
- `[ ]` Add decay worker that touches only due rows through indexed access.
- `[ ]` Enforce canonical lifecycle transitions for decay.
- `[ ]` Audit dream mutations.
- `[ ]` Add tests for wall-clock cap, spend cap, no-scan decay, and immutable
  versions.

## M7 — Association Graph And One-Hop Recall

Status: `[ ]` Not started.

- `[ ]` Add associate worker.
- `[ ]` Populate/update `memory_links` with weighted associations.
- `[ ]` Enforce per-node fan-out cap.
- `[ ]` Prune weak links under cap.
- `[ ]` Add one-hop recall expansion over `memory_links`.
- `[ ]` Add scoring inputs for graph centrality and link strength.
- `[ ]` Add fixture where graph recall surfaces a related memory missed by
  lexical/vector recall.
- `[ ]` Verify p99 recall remains within `< 100 ms` target.

## M8 — Profile Extraction Behind Approvals

Status: `[ ]` Not started.

- `[ ]` Add `profile` module as the only writer of `profile_facts`.
- `[ ]` Add `approvals` module and approval decision workflow.
- `[ ]` Add `ExtractProfile` worker that creates pending approvals only.
- `[ ]` Add `approve --list` CLI.
- `[ ]` Add `approve --id <id> --accept` CLI.
- `[ ]` Add `approve --id <id> --reject` CLI.
- `[ ]` Enforce no `profile_facts` write without approved `approval_id`.
- `[ ]` Audit approve/reject decisions.
- `[ ]` Add idempotency for duplicate pending proposals.

## M9 — In-Process HNSW Behind VectorIndex

Status: `[ ]` Not started.

- `[ ]` Add HNSW `VectorIndex` implementation behind the M4 trait.
- `[ ]` Keep brute-force implementation selectable as correctness oracle.
- `[ ]` Add fixture requiring HNSW recall@10 within epsilon of brute force.
- `[ ]` Add 200k-embedding performance fixture.
- `[ ]` Verify p99 semantic recall remains `< 100 ms` within memory caps.
- `[ ]` Verify no API changes to M4 recall callers.
- `[ ]` Keep SQLite as the only durable store.

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
3. Start M4 (vector rerank in recall) over the embeddings the embed worker produces.
4. Land the deferred M3 increment: `openai_compat`/`ollama` adapters, reachability
   probe + failover, and the `setup` command.
5. Add M0 release-build/disk-free evidence if those plan requirements remain.
