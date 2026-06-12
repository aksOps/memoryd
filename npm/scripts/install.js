#!/usr/bin/env node
// Postinstall: download the prebuilt memoryd binary for this platform from
// the GitHub release matching this package version, verify its sha256
// against the published checksum, and unpack it next to the JS shim.
// Zero runtime dependencies; Node >= 18 (global fetch).
"use strict";

const crypto = require("node:crypto");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const { execFileSync } = require("node:child_process");

const PKG = require(path.join(__dirname, "..", "package.json"));
const TARGETS = {
  "linux-x64": "x86_64-unknown-linux-gnu",
  "linux-arm64": "aarch64-unknown-linux-gnu",
  "darwin-x64": "x86_64-apple-darwin",
  "darwin-arm64": "aarch64-apple-darwin",
};

function fail(msg) {
  console.error(`memoryd install: ${msg}`);
  process.exit(1);
}

async function fetchBytes(url) {
  const res = await fetch(url, { redirect: "follow" });
  if (!res.ok) {
    throw new Error(`GET ${url} -> ${res.status} ${res.statusText}`);
  }
  return Buffer.from(await res.arrayBuffer());
}

async function main() {
  if (process.env.MEMORYD_SKIP_DOWNLOAD === "1") {
    console.log("memoryd install: MEMORYD_SKIP_DOWNLOAD=1, skipping binary download");
    return;
  }

  const key = `${os.platform()}-${os.arch()}`;
  const target = TARGETS[key];
  if (!target) {
    fail(
      `unsupported platform ${key}; prebuilt binaries cover ` +
        `${Object.keys(TARGETS).join(", ")}. Build from source instead: ` +
        "https://github.com/aksOps/memoryd#obtain-and-build"
    );
  }

  // Override for testing the installer against a local HTTP server.
  const base =
    process.env.MEMORYD_BINARY_URL_BASE ||
    `https://github.com/aksOps/memoryd/releases/download/v${PKG.version}`;
  const asset = `memoryd-${target}.tar.gz`;

  console.log(`memoryd install: downloading ${base}/${asset}`);
  const [archive, checksumLine] = await Promise.all([
    fetchBytes(`${base}/${asset}`),
    fetchBytes(`${base}/${asset}.sha256`).then((b) => b.toString("utf8")),
  ]);

  const expected = checksumLine.trim().split(/\s+/)[0];
  const actual = crypto.createHash("sha256").update(archive).digest("hex");
  if (!expected || expected !== actual) {
    fail(`sha256 mismatch for ${asset}: expected ${expected}, got ${actual}`);
  }

  const nativeDir = path.join(__dirname, "..", "native");
  fs.rmSync(nativeDir, { recursive: true, force: true });
  fs.mkdirSync(nativeDir, { recursive: true });
  const archivePath = path.join(nativeDir, asset);
  fs.writeFileSync(archivePath, archive);
  // tar(1) is present on every supported platform (linux, darwin).
  execFileSync("tar", ["-xzf", archivePath, "-C", nativeDir]);
  fs.rmSync(archivePath);

  const bin = path.join(nativeDir, "memoryd");
  if (!fs.existsSync(bin)) {
    fail(`archive ${asset} did not contain a 'memoryd' binary`);
  }
  fs.chmodSync(bin, 0o755);
  console.log(`memoryd install: ${bin} ready (sha256 ${actual.slice(0, 12)}…)`);
}

main().catch((err) => fail(err.message));
