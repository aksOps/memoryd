# Operations Runbook

Backup, portability, file hygiene, and the negative-test strategy for the
security gates (roadmap E2 + F2).

## Backup and portability

Everything durable lives in one SQLite file (default
`~/.local/share/memoryd/memoryd.db`): events, memories, the association
graph, embeddings, jobs, profile facts, approvals, spend ledger, audit log.
The embedding model is compiled into the binary, and runtime secrets
(tokens, API keys) are deliberately *not* in the database — copying it never
leaks credentials.

**Cold copy (preferred):** SIGTERM the daemon (it drains gracefully and the
WAL checkpoints away on close), then copy the single `.db` file. SQLite files
are portable across OS and architecture.

**Live copy:** never copy just the main file while the daemon runs — use the
built-in consistent snapshot instead:

```bash
memoryd backup --to /backups/memoryd-$(date +%F).db
```

(`VACUUM INTO` under the hood: transactionally consistent, compacted, refuses
to overwrite.) If you must copy raw files while running, take `memoryd.db`,
`memoryd.db-wal`, and `memoryd.db-shm` together.

**Restore / migrate:** point any same-or-newer `memoryd` binary at the copied
file; migrations are idempotent and upgrade in place. One caveat: embeddings
are tagged with the model that produced them. The same binary resumes
semantic recall immediately; switching embedding models leaves lexical recall
and the graph fully working while new captures embed with the new model.

## File hygiene

`memoryd doctor --fix` applies the safe, reversible repairs:
`wal_checkpoint(TRUNCATE)` + `PRAGMA optimize`. `doctor` also reports
`disk_free_mb` for the database directory. For long-lived stores, enable the
retention horizons (see README "Retention") rather than pruning by hand.

## Negative-test strategy for the dependency gates (M0 evidence)

The CI gates (`cargo-deny`, `cargo-audit`, SBOM existence) are verified
without committing a deliberately vulnerable dependency:

1. **Advisory gate:** `cargo-audit` is exercised against the committed
   `Cargo.lock` plus a documented ignore list (`.cargo/audit.toml`) — the
   gate's failure path is proven whenever a new advisory lands upstream and
   CI goes red until triaged (this has occurred and is the accepted
   evidence; see RUSTSEC-2024-0436's documented ignore).
2. **License/ban gate:** `cargo-deny`'s failure path is proven by the
   targeted exception workflow: adding a dependency whose license is outside
   the allowlist (e.g. webpki-roots, CDLA-Permissive-2.0) fails the gate
   until an explicit, justified exception is committed to `deny.toml`.
3. **SBOM gate:** CI fails if `bom.json` is absent; deleting the generation
   step locally and running the verification step reproduces the failure.

Decision (recorded): no synthetic vulnerable crate is committed to the repo —
the gates' failure paths are demonstrated by real upstream events and the
exception workflow above, which keeps the dependency tree clean.

## Release-build evidence (M0 decision, recorded)

Pre-release CI builds and tests in debug profile only; `cargo build
--release` evidence is deferred to the M10 packaging milestone where release
artifacts are actually produced. Rationale: a release-profile build gate
without release consumers adds CI minutes but no user-facing assurance.
