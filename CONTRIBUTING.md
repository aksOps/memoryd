# Contributing

memoryd accepts changes through reviewable pull requests on the public source
repository. Use issues for bug reports, enhancement requests, and design
discussion.

## Contribution Process

1. Open an issue for substantial behavior changes before implementation.
2. Keep changes small and focused.
3. Add or update automated tests for new functionality and bug fixes.
4. Run the local verification commands before submitting.
5. Submit a pull request that explains the user-visible change and any security
   or compatibility impact.

## Development Setup

Install the pinned Rust toolchain from `rust-toolchain.toml`, then run:

```bash
cargo build --workspace --locked
cargo test --workspace --locked
```

## Required Checks

Run these before proposing a change:

```bash
cargo fmt --all -- --check
cargo build --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
bash scripts/bootstrap-security-tools.sh
.tools/security/bin/cargo-deny check advisories bans licenses sources
.tools/security/bin/cargo-audit audit --deny warnings
```

## Coding Standards

Rust code must compile on the pinned toolchain with `unsafe_code = "forbid"`.
Prefer small modules, explicit errors, bounded work, and local-first behavior.
Runtime code must not call public internet services unless the user explicitly
configures a provider adapter in the future.

## Test Policy

Major new functionality, bug fixes, parsers, and security-sensitive behavior
must include automated tests. Tests should exercise observable behavior through
public interfaces, not private implementation details.

## License

Unless stated otherwise, contributions are licensed under `MIT OR Apache-2.0`,
matching the project license.
