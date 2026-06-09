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

No public runtime vulnerabilities have been fixed in a release yet.
