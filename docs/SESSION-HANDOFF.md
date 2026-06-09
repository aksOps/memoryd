# Session Handoff

## Current State

This repository was created as the working directory for a new clean-room Rust memory daemon project. The project directory was empty when this handoff was written.

The user has already sent the planning instructions to Opus 4.8 xhigh/dynamic planning mode. Opus is expected to write the detailed plan to:

`docs/ARCHITECTURE-PLAN.md`

## Last Action

Created durable project context files under `docs/` so future work can continue from this repository without relying on the prior chat session.

## Next Action

When `docs/ARCHITECTURE-PLAN.md` exists, read it and compare it against `docs/PROJECT-CONTEXT.md`.

Then produce an implementation roadmap with small vertical slices. Do not start implementation until the plan is reviewed for constraint violations.

## Why

The plan is being generated externally by a high-reasoning model. The next useful step is to validate that plan against the non-negotiable constraints, then convert it into executable slices.

## Critical Constraints

- Build in Rust.
- Clean-room implementation only.
- Do not fork, copy source, copy docs, or imitate exact APIs from Shodh-Memory, agentmemory, or claude-mem.
- No Docker requirement.
- No Postgres.
- SQLite is the only required durable store.
- Remote LLM and remote embeddings only by default.
- Existing Ollama Pro and OpenCode-accessible LLM access may be used, but no additional pay-per-token/API spend by default.
- The daemon is a helper process, not a second agent runtime.
- All expensive work must be queued, batched, bounded, and resource-governed.
- Security, CVE avoidance, SBOM, dependency auditing, and maintainability are first-class requirements.

## Important Files

- `docs/PROJECT-CONTEXT.md`: durable product constraints and requirements from the planning session.
- `docs/DYNAMIC-PLAN-LAUNCHER-PROMPT.txt`: launcher prompt used for Opus dynamic planning.
- `docs/ARCHITECTURE-PLAN.md`: expected output from Opus; may not exist yet.
- `/mnt/gdrive/dev/memoryd/`: Google Drive artifact root for portable prompts, exports, benchmark outputs, and release-adjacent files.

## Do Not

- Do not add Postgres or Docker to the core design.
- Do not make local models required.
- Do not make paid API spend the default.
- Do not design an always-on LLM loop.
- Do not silently rewrite identity/profile memory without approval.
- Do not prioritize feature richness over bounded resource behavior.

## Open Threads

- Decide final crate layout after reading `docs/ARCHITECTURE-PLAN.md`.
- Decide whether vector search starts with SQLite blob scanning, `sqlite-vec`, or a pluggable abstraction.
- Decide which remote provider adapter to implement first: Ollama-compatible, OpenCode-accessible, or generic OpenAI-compatible.
- Decide initial benchmark subset after plan review.
- Keep using `pnpm` for Node/package-manager workflows. npm is still the registry target for binary-wrapper distribution.
