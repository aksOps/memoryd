#!/usr/bin/env node
// Thin shim: locate the platform-specific binary package (installed as an
// optionalDependency — npm fetches only the one matching this host) and
// exec it with inherited stdio so the CLI, HTTP daemon, and MCP stdio modes
// all behave as if the binary were invoked directly. No install scripts,
// no network access: the binary ships inside the platform package.
"use strict";

const { spawn } = require("node:child_process");

const PLATFORM_PACKAGES = {
  "linux-x64": "@aksops/memoryd-linux-x64",
  "linux-arm64": "@aksops/memoryd-linux-arm64",
  "darwin-x64": "@aksops/memoryd-darwin-x64",
  "darwin-arm64": "@aksops/memoryd-darwin-arm64",
};

function resolveBinary() {
  const key = `${process.platform}-${process.arch}`;
  const pkg = PLATFORM_PACKAGES[key];
  if (!pkg) {
    console.error(
      `memoryd: unsupported platform ${key}; prebuilt binaries cover ` +
        `${Object.keys(PLATFORM_PACKAGES).join(", ")}. Build from source: ` +
        "https://github.com/aksOps/memoryd#obtain-and-build"
    );
    process.exit(1);
  }
  try {
    return require.resolve(`${pkg}/memoryd`);
  } catch {
    console.error(
      `memoryd: platform package ${pkg} is not installed. It is an ` +
        "optionalDependency of @aksops/memoryd — reinstall without " +
        "--no-optional / --omit=optional, and ensure your registry mirror " +
        `serves ${pkg}.`
    );
    process.exit(1);
  }
}

const child = spawn(resolveBinary(), process.argv.slice(2), {
  stdio: "inherit",
});
for (const sig of ["SIGINT", "SIGTERM", "SIGHUP"]) {
  process.on(sig, () => child.kill(sig));
}
child.on("exit", (code, signal) => {
  if (signal) {
    process.kill(process.pid, signal);
  } else {
    process.exit(code === null ? 1 : code);
  }
});
child.on("error", (err) => {
  console.error(`memoryd: failed to start native binary: ${err.message}`);
  process.exit(1);
});
