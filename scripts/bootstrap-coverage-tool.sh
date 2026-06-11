#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TOOLS_DIR="${MEMORYD_COVERAGE_TOOLS_DIR:-${ROOT_DIR}/.tools/coverage}"
CACHE_DIR="${TOOLS_DIR}/cache"
BIN_DIR="${TOOLS_DIR}/bin"

mkdir -p "${CACHE_DIR}" "${BIN_DIR}"

require_cmd() {
    if ! command -v "$1" >/dev/null 2>&1; then
        printf 'missing required command: %s\n' "$1" >&2
        exit 1
    fi
}

require_cmd gh
require_cmd sha256sum
require_cmd tar
require_cmd uname

OS="$(uname -s)"
ARCH="$(uname -m)"

case "${OS}:${ARCH}" in
    Linux:x86_64)
        CARGO_LLVM_COV_TAG="v0.8.7"
        CARGO_LLVM_COV_ASSET="cargo-llvm-cov-x86_64-unknown-linux-musl.tar.gz"
        CARGO_LLVM_COV_SHA="967b5cc996c29d8baa52bbb4595ef1f53af35255af8e2036ddbc6468d7b523c7"
        ;;
    *)
        printf 'unsupported host for prebuilt coverage tool: %s %s\n' "${OS}" "${ARCH}" >&2
        printf 'fall back to: cargo install cargo-llvm-cov --locked\n' >&2
        exit 1
        ;;
esac

dest="${CACHE_DIR}/${CARGO_LLVM_COV_ASSET}"
if [ ! -f "${dest}" ]; then
    gh release download "${CARGO_LLVM_COV_TAG}" -R taiki-e/cargo-llvm-cov \
        --pattern "${CARGO_LLVM_COV_ASSET}" --dir "${CACHE_DIR}"
fi

printf '%s  %s\n' "${CARGO_LLVM_COV_SHA}" "${dest}" | sha256sum -c - >/dev/null

# The archive contains the bare binary at its root (no directory prefix).
tar -xzf "${dest}" -C "${BIN_DIR}"
chmod 0755 "${BIN_DIR}/cargo-llvm-cov"

"${BIN_DIR}/cargo-llvm-cov" llvm-cov --version

printf 'coverage tool installed in %s\n' "${BIN_DIR}"
