#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TOOLS_DIR="${MEMORYD_SECURITY_TOOLS_DIR:-${ROOT_DIR}/.tools/security}"
CACHE_DIR="${TOOLS_DIR}/cache"
EXTRACT_DIR="${TOOLS_DIR}/extract"
BIN_DIR="${TOOLS_DIR}/bin"

mkdir -p "${CACHE_DIR}" "${EXTRACT_DIR}" "${BIN_DIR}"

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
        CARGO_AUDIT_ASSET="cargo-audit-x86_64-unknown-linux-musl-v0.22.2.tgz"
        CARGO_AUDIT_SHA="7fb9497f8594b389e5fce5ef9b92db08432996895b2e0c5a0167a69ed445c428"
        CARGO_AUDIT_DIR="cargo-audit-x86_64-unknown-linux-musl-v0.22.2"

        CARGO_DENY_ASSET="cargo-deny-0.19.8-x86_64-unknown-linux-musl.tar.gz"
        CARGO_DENY_SHA="70e769ae3872e34d45132b17040859175e11401dc12dddb0303e0b8c7d088f3f"
        CARGO_DENY_DIR="cargo-deny-0.19.8-x86_64-unknown-linux-musl"

        CARGO_CYCLONEDX_ASSET="cargo-cyclonedx-x86_64-unknown-linux-musl.tar.xz"
        CARGO_CYCLONEDX_SHA="9bd3e599314f50810c9d98b8b68a617ff9d3cc20873968d90b29d121f6b226ff"
        CARGO_CYCLONEDX_DIR="cargo-cyclonedx-x86_64-unknown-linux-musl"
        ;;
    *)
        printf 'unsupported host for prebuilt security tools: %s %s\n' "${OS}" "${ARCH}" >&2
        printf 'fall back to: cargo install cargo-deny cargo-audit cargo-cyclonedx --locked\n' >&2
        exit 1
        ;;
esac

download() {
    repo="$1"
    tag="$2"
    asset="$3"
    dest="${CACHE_DIR}/${asset}"

    if [ ! -f "${dest}" ]; then
        gh release download "${tag}" -R "${repo}" --pattern "${asset}" --dir "${CACHE_DIR}"
    fi
}

verify() {
    asset="$1"
    sha="$2"
    printf '%s  %s\n' "${sha}" "${CACHE_DIR}/${asset}" | sha256sum -c - >/dev/null
}

extract() {
    asset="$1"
    dirname="$2"
    binary="$3"
    archive="${CACHE_DIR}/${asset}"
    out_dir="${EXTRACT_DIR}/${dirname}"

    rm -rf "${out_dir}"
    mkdir -p "${out_dir}"

    case "${asset}" in
        *.tar.gz|*.tgz) tar -xzf "${archive}" -C "${out_dir}" --strip-components 1 ;;
        *.tar.xz) tar -xJf "${archive}" -C "${out_dir}" --strip-components 1 ;;
        *)
            printf 'unsupported archive format: %s\n' "${asset}" >&2
            exit 1
            ;;
    esac

    chmod 0755 "${out_dir}/${binary}"
    ln -sf "${out_dir}/${binary}" "${BIN_DIR}/${binary}"
}

download rustsec/rustsec "cargo-audit/v0.22.2" "${CARGO_AUDIT_ASSET}"
download EmbarkStudios/cargo-deny "0.19.8" "${CARGO_DENY_ASSET}"
download CycloneDX/cyclonedx-rust-cargo "cargo-cyclonedx-0.5.9" "${CARGO_CYCLONEDX_ASSET}"

verify "${CARGO_AUDIT_ASSET}" "${CARGO_AUDIT_SHA}"
verify "${CARGO_DENY_ASSET}" "${CARGO_DENY_SHA}"
verify "${CARGO_CYCLONEDX_ASSET}" "${CARGO_CYCLONEDX_SHA}"

extract "${CARGO_AUDIT_ASSET}" "${CARGO_AUDIT_DIR}" cargo-audit
extract "${CARGO_DENY_ASSET}" "${CARGO_DENY_DIR}" cargo-deny
extract "${CARGO_CYCLONEDX_ASSET}" "${CARGO_CYCLONEDX_DIR}" cargo-cyclonedx

"${BIN_DIR}/cargo-audit" --version
"${BIN_DIR}/cargo-deny" --version
"${BIN_DIR}/cargo-cyclonedx" cyclonedx --version

printf 'security tools installed in %s\n' "${BIN_DIR}"
