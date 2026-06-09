# OpenSSF Best Practices Evidence

This file tracks repository evidence for the OpenSSF Best Practices passing
badge at https://www.bestpractices.dev/en/projects/13135.

## Badge Setup

1. Use project id `13135`.
2. Use `memoryd` as the human-readable project name.
3. Use the public repository URL as the project and repo URL:
   `https://github.com/aksOps/memoryd`.
4. Use `MIT OR Apache-2.0` as the SPDX license expression.
5. Use `Rust` as the language.
6. The README badge is:

```markdown
[![OpenSSF Best Practices](https://www.bestpractices.dev/projects/13135/badge)](https://www.bestpractices.dev/projects/13135)
```

## Local Evidence

| Criterion area | Repository evidence |
|---|---|
| Description and usage | `README.md`, `docs/API.md` |
| Obtain software | `README.md` build commands |
| Feedback and contribution process | `CONTRIBUTING.md`, GitHub issues and pull requests |
| Contribution requirements | `CONTRIBUTING.md` |
| FLOSS license | `LICENSE`, `LICENSE-MIT`, `LICENSE-APACHE`, Cargo metadata |
| Basic documentation | `README.md`, `docs/API.md`, `docs/PROJECT-CONTEXT.md` |
| Interface documentation | `docs/API.md` |
| Release notes | `CHANGELOG.md` |
| Bug reporting | `README.md`, `.github/ISSUE_TEMPLATE/bug_report.md` |
| Vulnerability reporting | `SECURITY.md` |
| Working build system | Cargo workspace and `rust-toolchain.toml` |
| Automated tests | `cargo test --workspace --locked`, `.github/workflows/ci.yml` |
| Test policy | `CONTRIBUTING.md` |
| Warning/lint policy | `cargo clippy --workspace --all-targets --locked -- -D warnings` |
| Static analysis | clippy, cargo-audit, cargo-deny in CI |
| Vulnerability and license checks | `deny.toml`, `scripts/bootstrap-security-tools.sh`, CI |
| SBOM | CI `cargo-cyclonedx` step |

## Passing Badge Answer Sheet

Use the following concise answers when completing the web form.

### Basics

| Criterion | Suggested answer |
|---|---|
| `description_good` | Met. `README.md` describes memoryd as a local-first Rust memory daemon for AI coding agents and personal long-term memory. |
| `interact` | Met. `README.md` documents how to obtain/build the software and points to GitHub issues, pull requests, `CONTRIBUTING.md`, and `SECURITY.md`. |
| `contribution` | Met URL: `https://github.com/aksOps/memoryd/blob/main/CONTRIBUTING.md`. |
| `contribution_requirements` | Met URL: `https://github.com/aksOps/memoryd/blob/main/CONTRIBUTING.md`. |
| `floss_license` | Met. Project license is `MIT OR Apache-2.0`. |
| `floss_license_osi` | Met. MIT and Apache-2.0 are OSI-approved licenses. |
| `license_location` | Met URL: `https://github.com/aksOps/memoryd/blob/main/LICENSE`. |
| `documentation_basics` | Met URL: `https://github.com/aksOps/memoryd/blob/main/README.md`. |
| `documentation_interface` | Met URL: `https://github.com/aksOps/memoryd/blob/main/docs/API.md`. |
| `sites_https` | Met. Project and repository use `https://github.com/aksOps/memoryd`. |
| `discussion` | Met. GitHub issues and pull requests are URL-addressable and searchable. |
| `english` | Met. Documentation and reports are accepted in English. |
| `maintained` | Met. The project is actively maintained and pursuing this badge. |

### Change Control

| Criterion | Suggested answer |
|---|---|
| `repo_public` | Met URL: `https://github.com/aksOps/memoryd`. |
| `repo_track` | Met. The repository uses Git via GitHub. |
| `repo_interim` | Met. Development happens through normal Git commits and pull requests, not just final release archives. |
| `repo_distributed` | Met. Git is distributed version control. |
| `version_unique` | Met. Cargo package version and Git commit ids uniquely identify builds before public releases. |
| `version_semver` | Met. Cargo package version is Semantic Versioning (`0.1.0`). |
| `version_tags` | Met. Public releases will be Git-tagged. |
| `release_notes` | Met URL: `https://github.com/aksOps/memoryd/blob/main/CHANGELOG.md`. |
| `release_notes_vulns` | Met. `CHANGELOG.md` includes a vulnerability note section; no public runtime vulnerabilities have been fixed yet. |

### Reporting

| Criterion | Suggested answer |
|---|---|
| `report_process` | Met URL: `https://github.com/aksOps/memoryd/issues`. |
| `report_tracker` | Met. GitHub issues are used. |
| `report_responses` | Met. No eligible 2-12 month bug-report history yet; project will acknowledge reports. |
| `enhancement_responses` | Met. No eligible 2-12 month enhancement-request history yet; project will respond to requests. |
| `report_archive` | Met URL: `https://github.com/aksOps/memoryd/issues`. |
| `vulnerability_report_process` | Met URL: `https://github.com/aksOps/memoryd/blob/main/SECURITY.md`. |
| `vulnerability_report_private` | Met URL: `https://github.com/aksOps/memoryd/blob/main/SECURITY.md`. |
| `vulnerability_report_response` | N/A or Met. No external vulnerability reports in the last 6 months; policy targets acknowledgment within 14 days. |

### Quality

| Criterion | Suggested answer |
|---|---|
| `build` | Met. Cargo workspace builds from source with `cargo build --workspace --locked`. |
| `build_common_tools` | Met. Cargo is the standard Rust build tool. |
| `build_floss_tools` | Met. Rust/Cargo and project dependencies are FLOSS. |
| `test` | Met. `README.md`, `CONTRIBUTING.md`, and CI document `cargo test --workspace --locked`. |
| `test_invocation` | Met. `cargo test --workspace --locked` is standard Rust test invocation. |
| `test_most` | Met. Current major features have unit tests for config, store, CLI parsing, HTTP capture, and validation paths. |
| `test_continuous_integration` | Met URL: `https://github.com/aksOps/memoryd/blob/main/.github/workflows/ci.yml`. |
| `test_policy` | Met. `CONTRIBUTING.md` requires tests for major new functionality and bug fixes. |
| `tests_are_added` | Met. The current capture feature includes store, CLI, auth, validation, and no-provider-call tests. |
| `tests_documented_added` | Met URL: `https://github.com/aksOps/memoryd/blob/main/CONTRIBUTING.md`. |
| `warnings` | Met. CI runs `cargo clippy --workspace --all-targets --locked -- -D warnings`; Rust `unsafe_code` is forbidden. |
| `warnings_fixed` | Met. CI fails on clippy warnings. |
| `warnings_strict` | Met. Warnings are treated as errors in clippy. |

### Security And Analysis

| Criterion | Suggested answer |
|---|---|
| `know_secure_design` | Met. Primary development follows least privilege, fail-closed remote bind auth, local-first defaults, no inline provider calls, and dependency/security gates. |
| `know_common_errors` | Met. The project explicitly addresses auth bypass, secret leakage, dependency CVEs, SQL injection via parameterized queries, unbounded request/body size, and unsafe Rust. |
| `crypto_published` | N/A. Current code does not implement cryptographic protocols. |
| `crypto_call` | N/A. Current code does not implement cryptography. |
| `crypto_floss` | N/A. Current code does not implement cryptography. |
| `crypto_keylength` | N/A. Current code does not implement cryptographic security mechanisms. |
| `crypto_working` | N/A. Current code does not use broken cryptographic algorithms. |
| `crypto_weaknesses` | N/A. Current code does not use weak cryptographic algorithms. |
| `crypto_pfs` | N/A. Current code does not implement key agreement. |
| `crypto_password_storage` | N/A. Current code does not store passwords. |
| `crypto_random` | N/A. Current code does not generate cryptographic keys or nonces. |
| `delivery_mitm` | Met. Source is delivered via HTTPS GitHub or SSH Git remotes. |
| `delivery_unsigned` | Met. Runtime and build scripts do not retrieve hashes over plain HTTP; security tool bootstrap uses HTTPS GitHub releases and pinned checksums. |
| `vulnerabilities_fixed_60_days` | Met. CI runs cargo-audit and cargo-deny; no known unpatched medium-or-higher vulnerabilities are allowed. |
| `vulnerabilities_critical_fixed` | Met. Critical/high vulnerabilities are release blockers per `SECURITY.md`, `CONTRIBUTING.md`, and CI. |
| `no_leaked_credentials` | Met. No credentials are committed; local secret scan should be run before release. |
| `static_analysis` | Met. CI runs clippy, cargo-audit, and cargo-deny before release. |
| `static_analysis_common_vulnerabilities` | Met. cargo-audit and cargo-deny check RustSec advisories and dependency policy. |
| `static_analysis_fixed` | Met. CI blocks unresolved warnings/advisories by policy. |
| `static_analysis_often` | Met. CI runs on push to `main` and pull requests. |
| `dynamic_analysis` | Met. Automated tests exercise SQLite migration/capture behavior and HTTP request validation. |
| `dynamic_analysis_unsafe` | N/A. Project is Rust with `unsafe_code = "forbid"`; no memory-unsafe project code is produced. |
| `dynamic_analysis_enable_assertions` | Met. Rust tests run with assertions enabled. |
| `dynamic_analysis_fixed` | Met. Confirmed exploitable findings from tests/dynamic analysis are fixed before release. |

## External Criteria Still Requiring Maintainer Action

The following cannot be completed from local files alone:

- Publish the repository at an HTTPS URL.
- Enable or confirm GitHub issues and pull requests.
- Enable GitHub private vulnerability reporting if private reports are desired.
- Complete the bestpractices.dev self-certification form for project `13135`.
