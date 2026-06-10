# Changelog

All notable user-visible changes will be documented in this file.

The project uses Semantic Versioning before the first stable release.

## Unreleased

- Added SQLite foundation with `doctor` and `stats` commands.
- Added fast capture plumbing for `remember` and `POST /v1/capture`.
- Added local lexical recall through `memoryd recall` and `POST /v1/recall`.
- Added deterministic best-effort redaction before raw event, provenance,
  metadata, and recall index persistence.
- Added CI checks for format, build, clippy, tests, dependency policy, advisory
  audit, and SBOM generation.
- Added OpenSSF Best Practices evidence in `.bestpractices.json`.
- Completed OpenSSF Best Practices passing badge self-certification.
- Hardened the HTTP server: per-connection socket timeouts (slowloris fix),
  thread-per-connection with a concurrency cap, per-IP auth-failure throttling,
  and startup rejection of empty or weak bearer tokens.
- Strengthened redaction: `fact_value` re-redaction in the approvals path,
  16+ digit-only secret detection, case-insensitive API-key prefixes; added
  `--token-file` and documented token-handling hygiene.
- Tightened the HTTP protocol surface: chunked transfer-encoding rejected,
  strict integer `ts_ms`, bounded duration caps.
- Added graceful shutdown (SIGTERM/SIGINT drain), `GET /v1/health`, structured
  stderr logging, and the single-writer `store::Writer` actor for hot-path
  writes.
- Added `memoryd mcp`: an MCP stdio server (protocol 2024-11-05, no network
  bind) exposing `memory_remember`, `memory_recall`, `memory_stats`, and the
  new `memory_graph` association-graph neighbors tool backed by
  `Store::memory_neighbors`.
- Added the generic `openai_compat` provider adapter (embeddings + chat
  completions against any OpenAI-shaped base URL: api.openai.com, Ollama,
  vLLM, LM Studio), replacing the provider-specific `ollama`/`opencode` stub
  names. Opt-in via `--adapter`/`MEMORYD_ADAPTER` + `MEMORYD_OPENAI_*` env
  settings; requires a non-zero `MEMORYD_SPEND_CAP_USD`; rustls TLS.

No public runtime vulnerabilities have been fixed in a release yet.
