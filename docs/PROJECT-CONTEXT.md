# Project Context: memoryd

## Product Thesis

`memoryd` is a clean-room Rust memory daemon for AI coding agents and personal long-term memory.

It should capture useful context from agent sessions, import historic data, build durable memories, clean up noise, strengthen useful associations, run scheduled dreaming/consolidation, and return only high-value context to future agents.

The system should feel like an adaptive personal memory substrate. It is not a full autonomous agent, not a heavy database stack, and not something that occupies the host machine.

## Conceptual Inspirations

Borrow concepts, not code:

- Shodh-Memory: adaptive relevance, activation, decay, association strengthening, tiered memory, self-cleanup.
- agentmemory: coding-agent hooks, MCP tools, cross-agent support, session capture, replayable provenance.
- claude-mem: session compression, handoff continuity, future-session context injection.

## Key Decisions

- Implementation language: Rust.
- Distribution target: single portable binary.
- npm distribution: wrapper package around prebuilt Rust binaries is desired.
- Use `pnpm` for Node/package-manager workflows going forward. npm remains the registry/distribution target, not the preferred local package manager.
- Use `/mnt/gdrive/dev/memoryd/` for portable project artifacts, prompt archives, exports, benchmark artifacts, and release-adjacent files where appropriate.
- Durable store: SQLite only.
- Remote LLM and embeddings: supported, but remote-provider only by default.
- No local model runtime requirement now or in the future.
- Existing entitlements such as Ollama Pro and OpenCode-accessible LLMs may be used as provider adapters.
- No additional pay-per-token/API spend by default.
- No Docker requirement.
- No Postgres.
- Clean-room implementation only.

## Current Implementation Status

Operational roadmap checklist: `docs/MILESTONE-TASKS.md`.

Implemented now:

- Rust workspace with `memoryd` binary and `memoryd-core` library.
- SQLite schema migrations through v2, including canonical tables and FTS over
  captured raw events.
- CLI commands: `doctor`, `stats`, `remember`, `recall`, and `serve`.
- REST endpoints: `POST /v1/capture` and `POST /v1/recall` on the local HTTP
  server.
- Deterministic best-effort redaction before metadata, payload, provenance, and
  recall index persistence.
- Safe `audit_log` rows for capture appends, redaction summaries, and HTTP auth
  rejections.
- Bounded capture admission using the configured queue-depth cap; saturated
  captures persist the raw event and return degraded without enqueueing an embed
  job.
- M1 capture latency fixtures are explicit ignored performance tests; latest
  local single-threaded p95 evidence is `2.226592ms` for core capture and
  `771.531µs` for HTTP-handler capture.
- M2 lexical recall is provider-free over `raw_events_fts` and is the accepted M2
  variant; durable `memories`/`memory_versions` recall is deferred to the
  dream/consolidation plane (M6). An ignored recall-latency fixture over a 50,000
  raw-event corpus (bounded ~200-row query) records p95 `87.664401ms` (p99
  `94.877763ms`, median `491.151µs`) against the plan's `< 100 ms` recall SLO; the
  p95/p99 tail varies with shared-host contention rather than recall cost, and the
  median is the contention-robust algorithm cost. Latency scales with match-set
  size since raw recall has no hard candidate cap (deferred to M6).
- M3 background plane: the `jobs` queue is leased exactly-once (atomic
  `UPDATE ... RETURNING`, visibility timeout, attempt accounting); a governor bounds
  admission (`queue_depth_max`) and per-tick worker batch size (`worker_concurrency`).
  The embed worker runs inside `serve` over a second WAL connection, embedding
  captured raw events via the deterministic `null` adapter and writing `embeddings`
  and `provider_usage` rows (`est_cost = 0`; paid adapters are blocked at the default
  zero spend cap). Remote adapters (`openai_compat`/`ollama`) and the `setup` command
  are deferred behind the in-place `ProviderAdapter` seam.
- M4 vector rerank: a `vectorindex` (brute-force cosine over the <=256 FTS-prefiltered
  shortlist) reranks recall by semantic similarity fused with bm25, with a query-embedding
  cache (`owner_type='query'`; cache hit = no provider call) and clean lexical degrade when
  no semantic signal is available. Gated on `ProviderAdapter::embeds_semantically`, so the
  default `null` adapter (hash, no signal) keeps recall lexical; production semantic recall
  activates with a real embedding provider. Uplift proven with a deterministic test double.
- M5 historic import: `import --source jsonl --path <p>` stages each line as a
  `kind='import'` raw event through the same capture path (sessions, FTS, embed queue).
  Idempotent by a `content_hash` partial-unique index (migration 0003; FNV-1a, not BLAKE3),
  resumable, and governor-bounded (pauses when the embed queue is full). Source-specific
  importers and distillation into `memories` are deferred to M6.
- M6 dream plane: `dream --now` (and a `serve` scheduler) consolidates pending raw_events
  into durable `memories` + immutable `memory_versions` (lexical dedup-cluster; LLM summary
  test-double-only) and decays due memories (`active→decaying→dormant→archived`, scan-free
  via `memories_decay_due`), under wall-clock (`partial`) and provider-spend (`budget_capped`)
  caps with `dream_runs` accounting. Migration 0004 adds `source_trust`/`decay_score`/
  `decay_recomputed_at` + a `consolidated_at` cursor. §9.7 cleanup/purge is deferred.
- M7 association graph + one-hop recall: a `dream` associate phase builds/reinforces/prunes
  symmetric `memory_links` (co-occurrence by `source_session`; embedding-similarity
  `semantic` via a test-double — `null` does co-occurrence only), with a per-node fan-out
  cap (≤32) + weak-floor prune. `recall <q> [--hops 0|1]` adds durable-memory recall over
  `memories_fts` + one-hop graph expansion, falling back to raw-event recall when no memory
  matches. `graph_centrality` (§9.4) is precomputed and folded into `R_base` (0.12) and
  `R_recall`. Migration 0005 adds `memories.centrality`/`source_session`. Link weight
  temporal-decay is deferred.
- M8 profile extraction behind the approvals gate (H6): a `dream` extract-profile phase
  proposes facts from durable memories (`kind ∈ {identity,preference,fact,decision}`)
  into `approvals(pending)` — never writing `profile_facts` directly (enforced by the
  `profile_facts.approval_id` NOT NULL FK). `approve [--list] [--id N --accept|--reject]`
  is the human gate: accept commits the fact (superseding any active fact for the key,
  citing the approval); reject writes nothing. Propose-once-per-`fact_key`; LLM
  extraction stubbed (deterministic kind-based, optional gated `summarize`). Migration
  0006 adds a pending-approvals target index. `remember` yields observations, so profile
  extraction targets typed captures (e.g. HTTP capture with `kind:"preference"`).
- CI/security gates for format, build, clippy, tests, dependency policy,
  advisory audit, and SBOM generation.
- OpenSSF Best Practices passing evidence.

Still planned:

- Background workers, governor admission, provider adapters, vector reranking,
  dreaming/consolidation, MCP/hook facades, approval-gated profile facts,
  broader worker/provider/profile audit coverage, portable snapshots, and npm
  binary distribution.

## Required Capabilities

- Remote server mode.
- MCP interface.
- REST API.
- Agent hooks for Claude Code, Codex, OpenCode, Cursor, and similar tools.
- Historic data import from Claude Code JSONL, agentmemory, claude-mem, Git history, notes, and chat exports.
- Dreaming/consolidation feature.
- Batched remote embeddings.
- Batched remote LLM summarization/reflection.
- No-additional-cost LLM mode using existing subscriptions/entitlements such as Ollama Pro and OpenCode-accessible models.
- Adaptive relevance scoring.
- Decay and cleanup of unnecessary information.
- Deduplication and merge of repeated memories.
- User profile / "how I think" model.
- Approval workflow for identity/profile changes.
- Secret redaction and privacy filtering.
- Audit trail and provenance.
- Export/import portable snapshots.
- Small-VM resource governor.
- Plugin architecture for optional future extensions, especially provider adapters and in-process/local embeddings, while keeping the default build remote-only and lightweight.
- Benchmark harness with LongMemEval-S-style retrieval evaluation.
- Security model with authentication, authorization, least-privilege defaults, dependency auditing, CVE scanning, SBOM generation, and release gates that block known unresolved critical/high vulnerabilities.
- Maintainability plan with small modules, clear interfaces, tests, docs, upgrade policy, dependency policy, and architectural decision records.

## Resource Invariants

The daemon must never grow out of proportion. It is a helper daemon, not a second agent runtime.

Design around:

- Bounded queues.
- Bounded workers.
- Bounded memory.
- Bounded CPU usage.
- Bounded dream runtime.
- Batch sizes for embeddings and LLM calls.
- Backpressure behavior.
- Idle/scheduled background processing.
- Fast capture path that only appends minimal events.
- Heavy work always deferred to workers.
- Graceful degradation when overloaded.

Default small-VM profile should assume:

- One worker.
- SQLite only.
- Remote LLM calls batched.
- Remote embedding calls batched.
- Dreaming scheduled, not continuous.
- Cleanup incremental.
- Recall fast and always available.
- No full database scans during normal operation.
- No additional paid API spend unless explicitly configured.

## Memory Lifecycle Ideas

Candidate states to consider:

- raw_event.
- candidate_memory.
- active_memory.
- reinforced_memory.
- stale_memory.
- superseded_memory.
- archived_memory.
- rejected_memory.

Profile and identity changes should require approval. Low-risk cleanup can be automatic only when confidence is high and provenance is retained.

## Dreaming Requirements

Dreaming means scheduled/idle consolidation, not autonomous agency.

It should:

- Replay recent events and sessions.
- Cluster related memories.
- Extract durable patterns and preferences.
- Strengthen useful links.
- Decay stale/noisy memories.
- Merge duplicates.
- Generate a dream journal.
- Queue uncertain inferences for approval.
- Respect strict runtime and provider budgets.

## Historic Import Requirements

Import should support:

- Claude Code JSONL.
- agentmemory export/API.
- claude-mem export/db.
- Git history.
- Markdown/Obsidian notes.
- ChatGPT/Claude exports.
- Generic JSONL.

Imported data should be staged as low-confidence candidate memories first. Dream/consolidation may promote it, but identity/profile updates require approval.

## Provider Strategy

Remote provider adapters should support:

- Remote embeddings.
- Remote LLM summarization/reflection.
- Ollama Pro or Ollama-compatible hosted endpoint.
- OpenCode-accessible models.
- OpenAI-compatible providers.
- Cached fixture providers for tests and benchmarks.

Strict spend guards are required:

- No new paid APIs enabled by default.
- Max calls per minute.
- Max tokens per job.
- Daily usage budget.
- Provider dry-run.
- Provider usage logging.

## Benchmark Plan Ideas

Benchmarks to consider:

- LongMemEval-S-style retrieval evaluation.
- LoCoMo-style long conversation memory.
- LaMP-style personalization/profile evaluation.
- BEIR/MTEB small retrieval subsets.
- HotpotQA or MuSiQue multi-hop retrieval subset.
- Synthetic small-VM stress tests.
- Import/dream throughput tests.
- Idle CPU/RAM tests.

Metrics should include:

- R@5.
- R@10.
- MRR.
- p50/p95 recall latency.
- Ingest throughput.
- Dream runtime.
- Queue delay.
- Memory footprint.
- Disk growth.
- Provider call count.
- Provider token usage.

Benchmark modes should include:

- Free-only retrieval mode.
- Cached embedding fixture mode.
- No-additional-cost LLM judge mode using existing entitlements.
- Provider-backed mode with explicit budget.

## Security And Maintainability Requirements

- Localhost-only bind by default.
- Auth required for non-loopback remote access.
- Secret redaction before persistence is implemented; provider-send redaction
  reports remain planned with the provider worker path.
- Capture/redaction/auth-rejection audit logging is implemented without storing
  original secret or bearer-token material.
- Secure provider credential storage.
- Audit logging for imports, dreams, approvals, deletes, and provider calls.
- Dependency policy for Rust crates and npm wrapper packages.
- CVE/advisory scanning in CI and release workflows.
- Release gates blocking known unresolved critical/high vulnerabilities.
- SBOM generation for release artifacts.
- License policy for dependencies.
- Supply-chain controls for npm binary packages and GitHub Releases.
- Plugin safety boundaries.
- Fuzz/property tests for parsers, importers, and redactors where appropriate.
- Small modules, clear interfaces, docs, test coverage, ADRs, and upgrade workflow.

Possible tools: `cargo audit`, `cargo deny`, OSV-Scanner, `gitleaks`, `npm audit`, SBOM generation, release checksums/signatures.

## Artifact Locations

- Local working project: `/home/dev/projects/memoryd`.
- Google Drive artifact root: `/mnt/gdrive/dev/memoryd`.
- Planning prompts: `/mnt/gdrive/dev/memoryd/prompts`.
