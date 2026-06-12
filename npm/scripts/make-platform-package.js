#!/usr/bin/env node
// CI helper (not shipped in any package): assemble a platform-specific
// binary package directory ready for `npm publish`.
//
//   node make-platform-package.js <out-dir> <npm-os> <npm-cpu> <binary-path>
//
// The package version is taken from ../package.json so the main package and
// every platform package always publish in lockstep.
"use strict";

const fs = require("node:fs");
const path = require("node:path");

const [outDir, osName, cpuName, binaryPath] = process.argv.slice(2);
if (!outDir || !osName || !cpuName || !binaryPath) {
  console.error(
    "usage: make-platform-package.js <out-dir> <npm-os> <npm-cpu> <binary-path>"
  );
  process.exit(1);
}

const main = require(path.join(__dirname, "..", "package.json"));
const name = `@aksops/memoryd-${osName}-${cpuName}`;

fs.mkdirSync(outDir, { recursive: true });
fs.copyFileSync(binaryPath, path.join(outDir, "memoryd"));
fs.chmodSync(path.join(outDir, "memoryd"), 0o755);

fs.writeFileSync(
  path.join(outDir, "package.json"),
  JSON.stringify(
    {
      name,
      version: main.version,
      description: `memoryd prebuilt binary for ${osName}-${cpuName} (installed via @aksops/memoryd)`,
      license: main.license,
      repository: main.repository,
      os: [osName],
      cpu: [cpuName],
      files: ["memoryd"],
      publishConfig: main.publishConfig,
    },
    null,
    2
  ) + "\n"
);

fs.writeFileSync(
  path.join(outDir, "README.md"),
  `# ${name}\n\nPrebuilt memoryd binary for ${osName}-${cpuName}. Do not install directly —\nit is an optionalDependency of [@aksops/memoryd](https://github.com/aksOps/memoryd).\n`
);

console.log(`assembled ${name}@${main.version} in ${outDir}`);
