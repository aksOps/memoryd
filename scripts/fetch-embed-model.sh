#!/usr/bin/env bash
# Fetch the bge-small-en-v1.5 fp32 ONNX + tokenizer that build.rs embeds into the
# binary (include_bytes!). Run once before building; air-gapped builds instead
# pre-place the same files (and build.rs re-verifies the pinned SHA-256 either way).
# Source: https://huggingface.co/Xenova/bge-small-en-v1.5 (MIT).
set -euo pipefail

DIR="$(cd "$(dirname "$0")/.." && pwd)/crates/memoryd-core/assets/bge-small-en-v1.5"
BASE="https://huggingface.co/Xenova/bge-small-en-v1.5/resolve/main"

declare -A FILES=(
  ["model.onnx"]="828e1496d7fabb79cfa4dcd84fa38625c0d3d21da474a00f08db0f559940cf35|$BASE/onnx/model.onnx"
  ["tokenizer.json"]="d241a60d5e8f04cc1b2b3e9ef7a4921b27bf526d9f6050ab90f9267a1f9e5c66|$BASE/tokenizer.json"
)

mkdir -p "$DIR"
for name in "${!FILES[@]}"; do
  sha="${FILES[$name]%%|*}"
  url="${FILES[$name]##*|}"
  dst="$DIR/$name"
  if [ -f "$dst" ] && echo "$sha  $dst" | sha256sum -c --status; then
    echo "ok: $name (cached, sha256 verified)"
    continue
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
  echo "$sha  $dst.tmp" | sha256sum -c --status || { echo "FATAL: sha256 mismatch for $name" >&2; rm -f "$dst.tmp"; exit 1; }
  mv -f "$dst.tmp" "$dst"
  echo "ok: $name (downloaded, sha256 verified)"
done
echo "embed model assets ready in $DIR"
