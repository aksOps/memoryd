# @aksops/memoryd

npm distribution of [memoryd](https://github.com/aksOps/memoryd) — a
local-first memory daemon for AI coding agents. The package is a thin shim:
`postinstall` downloads the prebuilt binary for your platform from the
matching GitHub release and verifies its sha256 before unpacking.

Supported platforms: linux x64/arm64, macOS x64/arm64. Anything else:
[build from source](https://github.com/aksOps/memoryd#obtain-and-build).

## Pre-release install (GitHub npm registry)

Pre-release versions are published to the GitHub npm registry, which
requires authentication even for public packages. Put this in `~/.npmrc`
(the token is a GitHub PAT with `read:packages`):

```
@aksops:registry=https://npm.pkg.github.com
//npm.pkg.github.com/:_authToken=YOUR_GITHUB_PAT
```

Then:

```bash
npm install -g @aksops/memoryd
memoryd --version
memoryd doctor
memoryd integrate --dry-run
```

Once validated, stable versions will be published to the public npmjs
registry under the same name (no `.npmrc` changes needed beyond removing
the registry override).

## Notes

- The native binary is self-contained (embedded embedding model, no system
  dependencies, rustls TLS — no OpenSSL).
- `npm` package version may carry a pre-release suffix (e.g. `0.1.0-pre.1`)
  while `memoryd --version` reports the underlying crate version (`0.1.0`).
- The shim forwards argv, stdio, exit codes, and signals, so `memoryd mcp`
  (stdio JSON-RPC) and `memoryd serve` work exactly as the bare binary.
