# Security Policy

## Supported Versions

memoryd has not made a public stable release yet. Security fixes are applied to
the main development branch until the first release line exists.

## Reporting Vulnerabilities

Report vulnerabilities through the repository's GitHub private vulnerability
reporting flow when it is available. If private reporting is not available yet,
open a GitHub issue with minimal public detail and ask for a private follow-up
channel before sharing exploit details, secrets, or sensitive logs.

The project aims to acknowledge vulnerability reports within 14 days.

## Security Expectations

The default runtime must be local-first and localhost-bound. Non-loopback binds
require a bearer token. Public-internet runtime calls are disabled by default.
Dependency advisories, license policy, and SBOM generation are checked in CI.
