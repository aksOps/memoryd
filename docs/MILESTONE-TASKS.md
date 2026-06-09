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
- `[~]` M1 is mostly implemented but missing latency evidence.
- `[~]` M2 has provider-free lexical recall, but not the full durable-memory
  recall path from the plan.
- `[ ]` M3 and later are not started beyond schema stubs and queued `embed` jobs.

Next implementation target: add M1 capture latency evidence, then close the M2
gaps that do not require providers.

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

Status: `[~]` Mostly implemented, not complete.

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
- `[ ]` Add a capture latency fixture or benchmark for 100 sequential inserts.
- `[ ]` Record p95 capture latency evidence against the `< 8 ms` M1 target.

## M2 — Lexical Recall, Provider-Free

Status: `[~]` Provider-free recall exists, but the plan's durable-memory recall
path is not complete.

- `[x]` `recall` CLI exists.
- `[x]` `POST /v1/recall` exists.
- `[x]` Recall is provider-free and local by default.
- `[x]` Recall searches redacted captured raw events through SQLite FTS5.
- `[x]` Recall treats operator-like FTS terms as literals.
- `[x]` Recall rejects empty or punctuation-only queries.
- `[x]` Recall does not write provider usage rows.
- `[ ]` Decide whether M2 should continue to recall over `raw_events_fts` for the
  near-term product, or switch now to the plan's `memories`/`memory_versions`
  durable-memory path.
- `[ ]` If switching to planned path, make `remember` create a durable `memories`
  row and immutable `memory_versions` v1.
- `[ ]` If staying on raw-event recall temporarily, update the roadmap to mark
  this as the accepted M2 variant.
- `[ ]` Add score breakdown and provenance to recall results if the plan's M2 API
  remains authoritative.
- `[ ]` Queue access-bump jobs instead of writing access stats inline.
- `[ ]` Add EXPLAIN/query-plan assertions proving FTS/index use.
- `[ ]` Add p99 recall benchmark evidence against the `< 100 ms` M2 target.

## M3 — Queue, Governor, Embed Worker, Provider Adapters

Status: `[ ]` Not started beyond `jobs` schema and capture enqueue.

- `[ ]` Add queue leasing from `jobs` with atomic `pending -> running` transition.
- `[ ]` Add visibility timeout and attempt accounting.
- `[ ]` Add exactly-once concurrency tests for job leasing.
- `[ ]` Add governor caps for queue depth, concurrency, per-worker memory, CPU
  share, dream wall-clock, and spend window.
- `[ ]` Add over-cap enqueue/admission tests proving backpressure without OOM.
- `[ ]` Add fixed worker enum with `Embed` active first.
- `[ ]` Add `null` provider adapter for offline CI and default no-spend mode.
- `[ ]` Add `openai_compat` provider adapter behind explicit config.
- `[ ]` Add `ollama` provider adapter behind explicit config.
- `[ ]` Add provider reachability probe and failover order.
- `[ ]` Write `embeddings` rows from `Embed` worker.
- `[ ]` Write `provider_usage` rows for every embed provider call.
- `[ ]` Enforce default `spend_window_usd = 0` so paid calls are blocked unless
  opted in.
- `[ ]` Add `setup` or equivalent provider config/reachability command.

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

1. Add capture latency fixture and evidence.
2. Decide M2 raw-event recall versus planned durable-memory recall.
3. Close chosen M2 recall gaps with tests and docs.
4. Start M3 queue leasing and governor caps.
5. Add provider-free queue/degraded-path regression tests as M3 grows.
