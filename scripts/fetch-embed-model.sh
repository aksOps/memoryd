#!/usr/bin/env bash
# Fetch the bge-small-en-v1.5 fp32 ONNX + tokenizer that build.rs embeds into the
# binary (include_bytes!). Run once before building; air-gapped builds instead
# pre-place the same files (and build.rs re-verifies the pinned SHA-256 either way).
# Source: https://huggingface.co/Xenova/bge-small-en-v1.5 (MIT).
# Portable across Linux and macOS (bash 3.2, shasum fallback).
set -euo pipefail

DIR="$(cd "$(dirname "$0")/.." && pwd)/crates/memoryd-core/assets/bge-small-en-v1.5"
BASE="https://huggingface.co/Xenova/bge-small-en-v1.5/resolve/main"

sha256_ok() {
    # sha256_ok <sha> <file> -> exit 0 when the digest matches.
    if command -v sha256sum >/dev/null 2>&1; then
        echo "$1  $2" | sha256sum -c --status
    else
        echo "$1  $2" | shasum -a 256 -c --status
    fi
}

fetch_one() {
    name="$1"
    sha="$2"
    url="$3"
    dst="$DIR/$name"

    if [ -f "$dst" ] && sha256_ok "$sha" "$dst"; then
        echo "ok: $name (cached, sha256 verified)"
        return 0
    fi
    echo "fetching $name ..."
    python3 - "$url" "$dst.tmp" <<'PY'
import sys, urllib.request
url, dst = sys.argv[1], sys.argv[2]
req = urllib.request.Request(url, headers={"User-Agent": "memoryd-build"})
with urllib.request.urlopen(req, timeout=300) as r, open(dst, "wb") as f:
    while True:
        chunk = r.read(1 << 20)
        if not chunk:
            break
        f.write(chunk)
PY
    if ! sha256_ok "$sha" "$dst.tmp"; then
        echo "FATAL: sha256 mismatch for $name" >&2
        rm -f "$dst.tmp"
        exit 1
    fi
    mv -f "$dst.tmp" "$dst"
    echo "ok: $name (downloaded, sha256 verified)"
}

mkdir -p "$DIR"
fetch_one "model.onnx" \
    "828e1496d7fabb79cfa4dcd84fa38625c0d3d21da474a00f08db0f559940cf35" \
    "$BASE/onnx/model.onnx"
fetch_one "tokenizer.json" \
    "d241a60d5e8f04cc1b2b3e9ef7a4921b27bf526d9f6050ab90f9267a1f9e5c66" \
    "$BASE/tokenizer.json"
echo "embed model assets ready in $DIR"
