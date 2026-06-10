//! Verifies and embeds the bge-small-en-v1.5 fp32 ONNX model + tokenizer
//! (`scripts/fetch-embed-model.sh` downloads them; air-gapped builds pre-place the
//! same files). The SHA-256 pins below are the single source of integrity truth —
//! a missing or tampered asset fails the build with instructions, never a fallback.

use sha2::{Digest, Sha256};
use std::path::PathBuf;

const ASSETS: &[(&str, &str)] = &[
    (
        "bge-small-en-v1.5/model.onnx",
        "828e1496d7fabb79cfa4dcd84fa38625c0d3d21da474a00f08db0f559940cf35",
    ),
    (
        "bge-small-en-v1.5/tokenizer.json",
        "d241a60d5e8f04cc1b2b3e9ef7a4921b27bf526d9f6050ab90f9267a1f9e5c66",
    ),
];

fn main() {
    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    for (rel, want) in ASSETS {
        let path = root.join("assets").join(rel);
        println!("cargo:rerun-if-changed={}", path.display());
        let bytes = std::fs::read(&path).unwrap_or_else(|e| {
            panic!(
                "embed-model asset missing: {} ({e}).\n\
                 Run scripts/fetch-embed-model.sh (or pre-place the file for \
                 air-gapped builds), then rebuild.",
                path.display()
            )
        });
        let got = hex(&Sha256::digest(&bytes));
        assert_eq!(
            &got,
            want,
            "sha256 mismatch for {} — expected {want}, got {got}. \
             Re-run scripts/fetch-embed-model.sh.",
            path.display()
        );
        let key = if rel.ends_with("model.onnx") {
            "MEMORYD_BGE_MODEL"
        } else {
            "MEMORYD_BGE_TOKENIZER"
        };
        println!("cargo:rustc-env={key}={}", path.display());
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
