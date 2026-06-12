# memoryd

[![OpenSSF Best Practices](https://www.bestpractices.dev/projects/13135/badge)](https://www.bestpractices.dev/projects/13135)

Clean-room Rust memory daemon for AI coding agents and personal long-term memory.

memoryd is a local-first helper daemon that captures useful context from coding-agent sessions into SQLite, defers expensive work to background jobs, and keeps runtime defaults conservative: localhost bind, no public-internet calls, `null` provider mode, and zero paid-provider spend.

This project is pre-release. The current binary provides SQLite bootstrap, diagnostics, stats, fast `remember`, lexical `recall`, deterministic redaction before persistence, capture/auth audit rows, and narrow local HTTP `POST /v1/capture` and `POST /v1/recall` endpoints.

## Obtain And Build

```bash
git clone https://github.com/aksOps/memoryd.git
cd memoryd
cargo build --workspace --locked
cargo test --workspace --locked
```

The pinned Rust toolchain is declared in `rust-toolchain.toml`.

### Install via npm (pre-release)

Pre-release builds are published to the **GitHub npm registry** as
`@aksops/memoryd`: a thin shim whose postinstall downloads the sha256-verified
prebuilt binary (linux/macOS, x64/arm64) from the matching GitHub release.
See `npm/README.md` for the `.npmrc` setup and install steps. Stable versions
will be published to the public npmjs registry once validated (M10).

## Current Commands

```bash
cargo run -p memoryd -- doctor --db /tmp/memoryd.db
cargo run -p memoryd -- stats --db /tmp/memoryd.db
cargo run -p memoryd -- remember "Prod migrations use flyway" --kind rule --tags ops,db --db /tmp/memoryd.db
cargo run -p memoryd -- recall "flyway migrations" --k 5 --db /tmp/memoryd.db
cargo run -p memoryd -- serve --db /tmp/memoryd.db --bind 127.0.0.1:7077
cargo run -p memoryd -- mcp --db /tmp/memoryd.db
```

## MCP

`memoryd mcp` speaks MCP (protocol revision `2024-11-05`) over stdio — newline-
delimited JSON-RPC 2.0, no network bind ever — so MCP clients can use the
memory store and its association graph directly. It exposes four tools:
`memory_remember`, `memory_recall` (durable-memory recall with one-hop graph
expansion), `memory_stats`, and `memory_graph` (typed, weighted neighbors of a
memory over `memory_links`). Client configuration:

```json
{
  "mcpServers": {
    "memoryd": {
      "command": "memoryd",
      "args": ["mcp"],
      "env": { "MEMORYD_DB": "/path/to/memoryd.db" }
    }
  }
}
```

Smoke test:

```bash
printf '%s\n%s\n%s\n' \
 '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"smoke","version":"0"}}}' \
 '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
 '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' \
 | memoryd mcp --db /tmp/memoryd.db
```

See `docs/API.md` for tool schemas, the trust model, and error mapping.

### Integrate with coding agents

`memoryd integrate` auto-discovers installed AI coding agents and wires
memoryd into each. Three modes:

- `--mode mcp` (default): register the MCP server (deliberate tools:
  `memory_recall`, `memory_graph`, `memory_profile`, ...) plus a session-end
  `dream` hook.
- `--mode hooks`: **no MCP** — the full hook suite instead: automatic capture
  (every prompt and tool result), automatic context injection (persona at
  `SessionStart`, recall hits per `UserPromptSubmit`), and the session-end
  `dream` pass. The agent needs no memory awareness at all.
- `--mode all`: both.

```bash
memoryd integrate                 # all detected agents, user scope, MCP mode
memoryd integrate --mode hooks    # push-based: capture + inject, no MCP
memoryd integrate --dry-run       # preview every change, write nothing
memoryd integrate --agent codex   # just one agent
memoryd integrate --scope project # write project configs in the cwd
memoryd integrate --db /path/db   # embed an explicit --db in the registration
```

Hook handlers (`memoryd hook tool|prompt|session-start`) read the agent's hook
JSON on stdin, never fail the host agent (errors exit 0, detail to stderr),
capture through the normal redaction pipeline, and inject via the
`hookSpecificOutput.additionalContext` envelope (Claude Code and Codex).
Hermes gets capture hooks (`post_tool_call`) but documents no injection
mechanism; OpenCode keeps the `session.idle` dream plugin.

Supported — each agent gets MCP registration plus its native session hook
running a `dream` consolidation pass: **Claude Code** (`.mcp.json`/
`~/.claude.json` + `SessionEnd` hook), **OpenCode** (`opencode.json` `mcp`
block + a `plugins/memoryd.js` plugin on `session.idle`), **Codex**
(`~/.codex/config.toml`: `[mcp_servers.memoryd]` + a `[[hooks.Stop]]` hook),
**Hermes** (`~/.hermes/config.yaml`: `mcp_servers` + an `on_session_end`
hook).

Safety: JSON configs are deep-merged (other servers and settings preserved);
TOML/YAML configs are appended only when memoryd's section is absent, otherwise
the exact stanza is printed for a one-line manual paste rather than risk
corrupting an existing multi-server config. Every modified file is backed up to
`<file>.memoryd.bak` first. An existing `plugins/memoryd.js` is never
overwritten (it may carry your edits), and hook installs are path-independent
idempotent: one memoryd dream hook per file even if the binary moved.

## Providers

The provider seam has three adapters: `null` (inert), `local` (default —
in-process bge-small embeddings, no network, no spend), and `openai_compat` —
one **generic** adapter for any endpoint speaking the OpenAI wire shape
(embeddings + chat completions). There are no provider-specific adapters:
Ollama, vLLM, LM Studio, llama.cpp, and api.openai.com are all just a base
URL. It is selected with `--adapter openai_compat` or `MEMORYD_ADAPTER`, and
requires an explicit non-zero `MEMORYD_SPEND_CAP_USD` at startup — network
providers are opt-in, never a default.

Local Ollama:

```bash
MEMORYD_SPEND_CAP_USD=0.01 \
MEMORYD_OPENAI_BASE_URL=http://127.0.0.1:11434/v1 \
MEMORYD_OPENAI_EMBED_MODEL=nomic-embed-text \
MEMORYD_OPENAI_CHAT_MODEL=llama3.2 \
memoryd serve --adapter openai_compat
```

OpenAI:

```bash
MEMORYD_SPEND_CAP_USD=1.00 \
MEMORYD_OPENAI_API_KEY_FILE=~/.config/memoryd/openai.key \
MEMORYD_OPENAI_USD_PER_1K=0.00002 \
memoryd serve --adapter openai_compat
```

`MEMORYD_OPENAI_API_KEY_FILE` is preferred over `MEMORYD_OPENAI_API_KEY` (the
file can be `chmod 0600`); the key is sent only as the `Authorization` header
and never logged or echoed into errors. The embed worker uses
`{base}/embeddings`; dream consolidation/profile summarization uses
`{base}/chat/completions`, gated by the spend cap with
`MEMORYD_OPENAI_USD_PER_1K` as the price signal (0 = free local runtime).
Provider error bodies are truncated before persistence. TLS is rustls-based
(no system OpenSSL).

HTTP capture example:

```bash
curl -sS -X POST http://127.0.0.1:7077/v1/capture \
  -H 'Content-Type: application/json' \
  -d '{"session_id":"session-1","agent":"claude","source":"tool_result","kind":"observation","payload":{"text":"WAL timeout fixed"}}'
```

HTTP recall example:

```bash
curl -sS -X POST http://127.0.0.1:7077/v1/recall \
  -H 'Content-Type: application/json' \
  -d '{"query":"WAL timeout","k":5}'
```

See `docs/API.md` for CLI and REST request/response details. See
`docs/MILESTONE-TASKS.md` for the current roadmap task checklist. To keep the
daemon running under systemd or launchd, see `docs/RUNNING-AS-A-SERVICE.md`.

## Security Defaults

- Runtime is local-first and does not call public internet services by default;
  the `openai_compat` provider is explicit opt-in (`--adapter`/`MEMORYD_ADAPTER`)
  and refuses to start without a non-zero spend cap.
- The default bind is `127.0.0.1:7077`.
- Any non-loopback bind requires a bearer token of at least 16 characters; empty
  tokens are rejected at startup on any bind.
- Each accepted connection is handled on its own thread with 10s socket
  read/write deadlines, and concurrent connections are capped (excess gets 503),
  so a stalled client cannot block other callers.
- Repeated failed bearer authentication is throttled per peer IP (5 failures per
  minute locks the peer out for a minute with 429 responses).
- Capture redacts common secret shapes before writing metadata, payloads, provenance, and recall index text to SQLite.
- Capture writes safe `audit_log` rows for capture append and redaction summaries; HTTP auth rejection writes a safe audit row without storing bearer token values.
- Capture appends redacted raw events and queues background work when below the queue-depth cap; saturated captures return degraded instead of calling providers or failing inline.
- Recall uses local SQLite FTS over redacted captured raw events; it does not call providers inline.
- Rust `unsafe` code is forbidden at workspace level.
- CI runs formatting, build, clippy with `-D warnings`, tests, dependency policy, advisory audit, SBOM generation, line-coverage enforcement (85% floor via cargo-llvm-cov), and a SonarQube Cloud scan whose quality gate must pass (one-time SonarCloud setup steps in `docs/PRODUCTION-READINESS-PLAN.md`).
- `main` is protected with required CI, up-to-date checks, linear history, no force pushes, no deletions, and conversation resolution.

The current redactor is deterministic and best-effort, not a guarantee that every possible secret format is detected. It replaces matched content with `[REDACTED]` before persistence and covers sensitive JSON keys, bearer-style credentials, common API-key prefixes (case-insensitive), private-key markers, emails, long all-digit runs (16+ digits, e.g. card numbers), and high-entropy token-like spans. Known limitation: secrets that arrive percent-encoded are split at `%xx` boundaries and are not decoded or reassembled before matching.

Token handling: prefer `MEMORYD_TOKEN` or `--token-file <path>` over `--token`. Command-line arguments are world-readable on Linux via `/proc/<pid>/cmdline`, while the environment is owner-only and a token file can be `chmod 0600`.

## Current Scope

Implemented today: local SQLite schema/migrations, `doctor`, `stats`, `remember`, `recall`, local HTTP capture/recall/health, the MCP stdio facade (`memoryd mcp` with graph querying), the `integrate` command and `hook` handlers (`memoryd hook tool|prompt|session-start`) for wiring coding agents, the generic `openai_compat` provider adapter, background embed/dream workers behind the single-writer actor, redaction before persistence, capture/auth audit rows, approval-gated profile facts, graceful shutdown, CI/security gates, and OpenSSF Best Practices passing evidence.

Still planned: broader worker/provider/profile audit coverage, npm binary distribution, and release packaging/prebuilt binaries (M10).

## Package Manager Rule

The Node wrapper lives in `npm/` (zero runtime dependencies; `pnpm-lock.yaml` committed per the workspace rule). Use `pnpm` for local Node workflows; `npm` is reserved for registry publication (`npm publish` runs in the release workflow only).

## Retention

By default memoryd keeps every raw event forever (~3-5KB per capture). Two
opt-in horizons shed raw traffic while keeping distilled knowledge — memories,
the association graph, sessions, and the audit log are never touched:

```bash
MEMORYD_RETAIN_RAW_DAYS=180 \
MEMORYD_RETAIN_RAW_EMBED_DAYS=60 \
memoryd serve
```

`MEMORYD_RETAIN_RAW_DAYS` deletes *consolidated* raw events (and their FTS/
embedding rows) older than the horizon; `MEMORYD_RETAIN_RAW_EMBED_DAYS` drops
only the raw-event embedding vectors. Both run as a bounded dream phase with
`retention.sweep` audit rows; 0 (default) disables deletion entirely.

## Development Checks

```bash
cargo fmt --all -- --check
cargo build --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

## Local Security Tools

Use the hash-pinned prebuilt security tools instead of compiling them locally:

```bash
bash scripts/bootstrap-security-tools.sh
.tools/security/bin/cargo-deny check advisories bans licenses sources
.tools/security/bin/cargo-audit audit --deny warnings
.tools/security/bin/cargo-cyclonedx cyclonedx --manifest-path crates/memoryd/Cargo.toml --format json --override-filename bom
```

The SBOM is written to `crates/memoryd/bom.json` and is ignored locally; CI verifies it exists.

## Feedback And Contributions

Use GitHub issues for bug reports and enhancement requests. Use pull requests for code changes. See `CONTRIBUTING.md` for the contribution process, required checks, coding standards, and test policy.

Report vulnerabilities through the process in `SECURITY.md`.

## License

Licensed under either `MIT` or `Apache-2.0`, at your option. See `LICENSE`, `LICENSE-MIT`, and `LICENSE-APACHE`.
