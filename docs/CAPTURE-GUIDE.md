# Capture Discipline Guide

Memory quality in determines persona quality out: the dream pipeline can only
consolidate, link, and induce patterns from what capture gives it. Five
habits make the difference (roadmap E1).

## Use kinds deliberately

`--kind` (CLI) / `kind` (HTTP/MCP) drives decay half-lives, profile
extraction, and heuristic induction:

| kind | half-life | extracted to profile? | feeds heuristics? |
| --- | --- | --- | --- |
| `identity` | never | yes | no |
| `preference` | 180d | yes | no |
| `fact` | 120d | yes | no |
| `decision` | 90d | yes | **yes** |
| `task`/`todo` | 21d | no | no |
| `ephemeral` | 3d | no | no |
| anything else → `observation` | 14d | no | no |

The two highest-leverage kinds are `decision` (heuristic induction reads
these) and `preference` (becomes an approval-gated profile fact).

## Capture decisions explicitly, as one event

Consolidation clusters near-duplicates; it does not stitch a narrative out of
fragments. The rationale behind a decision survives only if you capture it:

```bash
memoryd remember "Chose SQLite over Postgres because single-file portability \
beats server features here; revisit if multi-writer load appears" \
  --kind decision --tags arch,storage
```

One well-written `decision` event is worth fifty tool-result fragments.

## Keep session ids meaningful

Co-occurrence links — half the knowledge graph — come from events sharing a
`session_id`. Use one session id per work stream (one task, one
investigation), not one per process launch and not one forever. Sessions idle
for 30+ minutes with 3+ memories get distilled into a `session_summary`
narrative; trivial idle sessions are closed.

## Tag for retrieval, not taxonomy

Tags land in provenance and help future lexical recall. Two or three concrete
tags (`ops`, `flyway`, `auth`) beat deep hierarchies.

## Curate the approvals queue

`memoryd approve` is not admin chores — it is how the secondary brain learns.
Every accepted `preference`/`heuristic.*` fact becomes part of the persona
served by `memory_profile`; every rejection permanently retires that proposal
key. Review pending approvals weekly:

```bash
memoryd approve            # list pending
memoryd approve --id <id> --accept   # or --reject
```
