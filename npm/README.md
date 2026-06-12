# @aksops/memoryd

npm distribution of [memoryd](https://github.com/aksOps/memoryd) — a
local-first memory daemon for AI coding agents.

Everything ships **through the npm registry**: the prebuilt binary lives
inside a platform-specific package (`@aksops/memoryd-<os>-<cpu>`) declared
as an `optionalDependency`, so npm fetches exactly one binary package for
your platform. There are **no install scripts and no install-time
downloads** — installs work behind corporate registry proxies
(Nexus/Artifactory) with no github.com access, and with `--ignore-scripts`.

Supported platforms: linux x64/arm64, macOS x64/arm64. Anything else:
[build from source](https://github.com/aksOps/memoryd#obtain-and-build).

## Pre-release install (GitHub npm registry)

Pre-release versions are published to the GitHub npm registry, which
requires authentication even for public packages. Direct setup in
`~/.npmrc` (token is a GitHub PAT with `read:packages`):

```
@aksops:registry=https://npm.pkg.github.com
//npm.pkg.github.com/:_authToken=YOUR_GITHUB_PAT
```

Behind Nexus: create an npm *proxy* repository pointing at
`https://npm.pkg.github.com` (with the same PAT as HTTP auth), add it to
your npm group repository, and scope it: `@aksops:registry=<your-nexus-npm-group-url>`.

Then:

```bash
npm install -g @aksops/memoryd
memoryd --version
memoryd doctor
memoryd integrate --dry-run
```

Once validated, stable versions will be published to the public npmjs
registry under the same names — at that point the packages flow through any
stock npmjs proxy with no registry overrides at all.

## Notes

- The native binary is self-contained (embedded embedding model, no system
  dependencies, rustls TLS — no OpenSSL). Each platform package is ~85 MB
  compressed; npm downloads only the one matching your platform.
- Do not add `--omit=optional` / `--no-optional`: the platform packages are
  `optionalDependencies` (the standard prebuilt-binary pattern, as used by
  esbuild and swc), and omitting them leaves the shim with no binary.
- The `npm` package version may carry a pre-release suffix (e.g.
  `0.1.0-pre.1`) while `memoryd --version` reports the underlying crate
  version (`0.1.0`).
- The shim forwards argv, stdio, exit codes, and signals, so `memoryd mcp`
  (stdio JSON-RPC) and `memoryd serve` work exactly as the bare binary.
