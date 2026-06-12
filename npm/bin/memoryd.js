#!/usr/bin/env node
// Thin shim: exec the downloaded native memoryd binary with inherited stdio
// so the CLI, HTTP daemon, and MCP stdio modes all behave as if the binary
// were invoked directly.
"use strict";

const fs = require("node:fs");
const path = require("node:path");
const { spawn } = require("node:child_process");

const bin = path.join(__dirname, "..", "native", "memoryd");
if (!fs.existsSync(bin)) {
  console.error(
    "memoryd: native binary missing. Reinstall the package " +
      "(npm rebuild @aksops/memoryd or reinstall) to trigger the download."
  );
  process.exit(1);
}

const child = spawn(bin, process.argv.slice(2), { stdio: "inherit" });
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
