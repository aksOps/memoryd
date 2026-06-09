# memoryd

[![OpenSSF Best Practices](https://www.bestpractices.dev/projects/13135/badge)](https://www.bestpractices.dev/projects/13135)

Clean-room Rust memory daemon for AI coding agents and personal long-term memory.

memoryd is a local-first helper daemon that captures useful context from coding-agent sessions into SQLite, defers expensive work to background jobs, and keeps runtime defaults conservative: localhost bind, no public-internet calls, `null` provider mode, and zero paid-provider spend.

This project is pre-release. The current binary provides SQLite bootstrap, diagnostics, stats, fast `remember`, lexical `recall`, deterministic redaction before persistence, and narrow local HTTP `POST /v1/capture` and `POST /v1/recall` endpoints.

## Obtain And Build

```bash
git clone https://github.com/aksOps/memoryd.git
cd memoryd
cargo build --workspace --locked
cargo test --workspace --locked
```

The pinned Rust toolchain is declared in `rust-toolchain.toml`.

## Current Commands

```bash
cargo run -p memoryd -- doctor --db /tmp/memoryd.db
cargo run -p memoryd -- stats --db /tmp/memoryd.db
cargo run -p memoryd -- remember "Prod migrations use flyway" --kind rule --tags ops,db --db /tmp/memoryd.db
cargo run -p memoryd -- recall "flyway migrations" --k 5 --db /tmp/memoryd.db
cargo run -p memoryd -- serve --db /tmp/memoryd.db --bind 127.0.0.1:7077
```

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

See `docs/API.md` for CLI and REST request/response details.

## Security Defaults

- Runtime is local-first and does not call public internet services by default.
- The default bind is `127.0.0.1:7077`.
- Any non-loopback bind requires a bearer token.
- Capture redacts common secret shapes before writing metadata, payloads, provenance, and recall index text to SQLite.
- Capture only appends redacted raw events and queues background work; it does not call providers inline.
- Recall uses local SQLite FTS over redacted captured raw events; it does not call providers inline.
- Rust `unsafe` code is forbidden at workspace level.
- CI runs formatting, build, clippy with `-D warnings`, tests, dependency policy, advisory audit, and SBOM generation.
- `main` is protected with required CI, up-to-date checks, linear history, no force pushes, no deletions, and conversation resolution.

The current redactor is deterministic and best-effort, not a guarantee that every possible secret format is detected. It replaces matched content with `[REDACTED]` before persistence and covers sensitive JSON keys, bearer-style credentials, common API-key prefixes, private-key markers, emails, and high-entropy token-like spans.

## Current Scope

Implemented today: local SQLite schema/migrations, `doctor`, `stats`, `remember`, `recall`, local HTTP capture/recall, redaction before persistence, CI/security gates, and OpenSSF Best Practices passing evidence.

Still planned: background job workers, provider adapters, vector reranking, dreaming/consolidation, MCP/hook facades, approval-gated profile facts, audit-log entries for each redaction, and npm binary distribution.

## Package Manager Rule

There is no Node wrapper in S01. When Node/package workflows are added, use `pnpm` and commit `pnpm-lock.yaml`. `npm` is reserved for the eventual registry publication target, not local development workflows.

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
