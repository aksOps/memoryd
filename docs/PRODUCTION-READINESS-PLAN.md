# Production Readiness Plan

Status date: 2026-06-11. Baseline: `main` at `8b16693`, all CI gates green,
261 tests passing, line coverage 88.80% (cargo-llvm-cov, workspace).

This plan closes the gaps found by the 2026-06-11 production-readiness audit
and adds quality/coverage gating to CI. Part B ships with this change; Part A
items are queued.

## Part A — readiness gaps (shipped)

- [x] **A1. README scope refresh.** `README.md` "Still planned" lists
  `openai_compat` adapters and hook facades, both of which have shipped.
  Replace with the real remainder: broader worker/provider/profile audit
  coverage, npm binary distribution, release artifacts (M10).
- [x] **A2. Daemon-management docs.** Add example `systemd` user unit and
  `launchd` plist for running `memoryd serve` long-term (new
  `docs/RUNNING-AS-A-SERVICE.md`, linked from README and OPERATIONS).
- [x] **A3. Disk-full behavior note.** OPERATIONS.md: SQLite writes fail with
  `ENOSPC` → HTTP 500 / CLI error; `doctor` reports `disk_free_mb`; monitor
  proactively.
- [x] **A4. `--version` flag.** `memoryd --version` (and `-V`) printing
  `memoryd <CARGO_PKG_VERSION>`; today it errors with "unknown command".
- [x] **A5. SIGPIPE polish.** `memoryd doctor | head` panics with a
  broken-pipe backtrace after correct output. Route CLI stdout through a
  write that exits 0 on `BrokenPipe`.
- [x] **A6. API.md health example.** Shows `"schema_version": 2`; current is
  7. Use a placeholder (`<current schema version>`) so it cannot go stale.

## Part B — Sonar scan + coverage gate (this change)

- [x] **B1. Coverage measurement in CI.** New `coverage` job in
  `.github/workflows/ci.yml`: pinned toolchain + `llvm-tools`, sha256-pinned
  prebuilt `cargo-llvm-cov` (`scripts/bootstrap-coverage-tool.sh`, mirroring
  the security-tools bootstrap), produces `lcov.info` for the workspace.
- [x] **B2. 85% line-coverage floor enforced in CI.** `cargo llvm-cov report
  --fail-under-lines 85` fails the job below 85% regardless of Sonar
  configuration. Baseline today: 88.80%.
- [x] **B3. SonarQube Cloud scan.** `SonarSource/sonarqube-scan-action`
  (sha-pinned, v8.2.0) with `sonar.qualitygate.wait=true`, so a failing
  quality gate fails the pipeline. Configuration in
  `sonar-project.properties` (Rust analyzer; coverage imported via
  `sonar.rust.lcov.reportPaths=lcov.info`). The scan step is skipped with a
  workflow warning until the `SONAR_TOKEN` secret exists; the 85% floor (B2)
  is enforced either way.

### One-time setup required on SonarCloud / GitHub (cannot be done from the repo)

- [ ] **S1.** Create the project on <https://sonarcloud.io> for
  `aksOps/memoryd` (organization key `aksops`, project key `aksOps_memoryd`
  — if the wizard generates different keys, update
  `sonar-project.properties` to match) and disable Automatic Analysis
  (CI-based analysis is configured here; the two modes conflict, and
  Automatic Analysis does not support Rust).
- [ ] **S2.** Add the `SONAR_TOKEN` repository secret (SonarCloud → My
  Account → Security → generate token).
- [ ] **S3.** Quality gate at 85%: SonarCloud's built-in "Sonar way" gate
  checks 80% on new code only. Create a custom gate (e.g. `memoryd`) with
  *Coverage on overall code ≥ 85%* and *Coverage on new code ≥ 85%*, assign
  it to the project. CI fails on gate failure via B3.
- [ ] **S4.** Branch protection: add the `coverage` job to the required
  status checks on `main` so "gate must pass" is enforced at merge time, not
  just visible.

## Part C — pre-release npm distribution (shipped 2026-06-11)

- [x] **C1. npm wrapper package** (`npm/`): `@aksops/memoryd`, zero runtime
  dependencies. `postinstall` downloads the prebuilt binary for the host
  platform from the GitHub release matching the package version, verifies
  the published sha256, and unpacks it; the `memoryd` bin shim forwards
  argv/stdio/exit codes/signals so `serve` and `mcp` behave identically to
  the bare binary. Tested locally end to end (download, tamper rejection,
  exit-code passthrough).
- [x] **C2. Release workflow** (`.github/workflows/release.yml`): on `v*`
  tag or manual dispatch — creates a GitHub prerelease, builds
  linux x64/arm64 + macOS x64/arm64 binaries (pinned toolchain, sha256
  asset checksums, per-target smoke test), attaches assets, and publishes
  the npm package to the GitHub npm registry with the workflow token.
- [x] **C3. Portable model fetch**: `scripts/fetch-embed-model.sh` now runs
  on macOS runners (bash 3.2 compatible, `shasum` fallback), same pinned
  hashes.

Publish runbook: bump `npm/package.json` version → merge to main → run the
`release` workflow with tag `v<version>` (or push the tag). Consumers need
`@aksops:registry=https://npm.pkg.github.com` plus a `read:packages` PAT in
`~/.npmrc` (see `npm/README.md`). npmjs publication stays gated on manual
validation (M10).

### Notes

- Coverage headroom is 3.8 points. The thin spots are
  `crates/memoryd/src/hook.rs` (67% lines) and `crates/memoryd/src/main.rs`
  (77%); large additions to those files without tests are what would
  realistically breach the floor first.
- The coverage job re-runs the test suite instrumented; it runs in parallel
  with the `rust` job and does not lengthen the critical path materially.
- No Rust dependencies were added; `cargo-llvm-cov` is a prebuilt,
  hash-pinned CI tool like cargo-deny/cargo-audit/cargo-cyclonedx.
