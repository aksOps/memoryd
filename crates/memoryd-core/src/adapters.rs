//! Provider adapter seam plus a deterministic, offline `null` embedding adapter.
//!
//! M3 only ships the `null` adapter: it proves the worker/embedding/ledger path
//! end-to-end with no network and no spend. `openai_compat`/`ollama`/`opencode`
//! adapters land in a later slice behind this same trait.

use std::collections::hash_map::DefaultHasher;
use std::fmt;
use std::hash::{Hash, Hasher};

/// A provider behind the worker plane. Every adapter is entitlement-/budget-gated
/// at the call site; the seam itself only describes capabilities.
pub trait ProviderAdapter {
    /// Stable adapter identifier, matching the `provider_usage.adapter` enum.
    fn id(&self) -> &'static str;
    /// Model identifier recorded with each embedding and usage row.
    fn model_id(&self) -> &str;
    /// Embed each input text. One output vector per input, same order.
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, AdapterError>;
    /// Whether the adapter can currently serve requests.
    fn reachable(&self) -> bool;
    /// Whether this adapter's embeddings carry a usable semantic signal. The
    /// `null` adapter's hash vectors do not, so recall must not rerank by them.
    fn embeds_semantically(&self) -> bool {
        true
    }
}

/// Deterministic, dependency-free embedding adapter for the default no-spend profile.
#[derive(Debug, Clone)]
pub struct NullAdapter {
    dim: usize,
    model_id: String,
}

impl NullAdapter {
    /// Default 32-dimensional null adapter.
    pub fn new() -> Self {
        Self::with_dim(32)
    }

    /// Null adapter producing `dim`-dimensional vectors.
    pub fn with_dim(dim: usize) -> Self {
        Self {
            dim,
            model_id: format!("null-hash-{dim}"),
        }
    }
}

impl Default for NullAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl ProviderAdapter for NullAdapter {
    fn id(&self) -> &'static str {
        "null"
    }

    fn model_id(&self) -> &str {
        &self.model_id
    }

    fn reachable(&self) -> bool {
        true
    }

    fn embeds_semantically(&self) -> bool {
        false
    }

    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, AdapterError> {
        Ok(texts.iter().map(|text| embed_one(text, self.dim)).collect())
    }
}

/// `vector[i]` is a stable hash of `(text, i)` mapped into `[-1, 1]`.
/// Empty text embeds to the zero vector (a valid, distinguishable point).
fn embed_one(text: &str, dim: usize) -> Vec<f32> {
    if text.is_empty() {
        return vec![0.0; dim];
    }
    (0..dim)
        .map(|i| {
            let mut hasher = DefaultHasher::new();
            text.hash(&mut hasher);
            i.hash(&mut hasher);
            let raw = hasher.finish();
            ((raw as f64 / u64::MAX as f64) * 2.0 - 1.0) as f32
        })
        .collect()
}

/// Heuristic prompt-token estimate (~4 chars/token); avoids a tokenizer dependency.
pub fn prompt_token_estimate(text: &str) -> i64 {
    let chars = text.chars().count();
    chars.div_ceil(4) as i64
}

/// Failure from a provider call.
#[derive(Debug, Clone)]
pub enum AdapterError {
    Embed(String),
}

impl fmt::Display for AdapterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Embed(message) => write!(f, "embed failed: {message}"),
        }
    }
}

impl std::error::Error for AdapterError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_adapter_is_deterministic_reachable_and_dim() {
        let adapter = NullAdapter::new();
        assert_eq!(adapter.id(), "null");
        assert_eq!(adapter.model_id(), "null-hash-32");
        assert!(adapter.reachable());

        let texts = vec!["busy_timeout fixed WAL contention".to_string()];
        let first = adapter.embed(&texts).expect("embed succeeds");
        let second = adapter.embed(&texts).expect("embed succeeds");

        assert_eq!(first[0].len(), 32);
        assert_eq!(first, second, "null adapter must be deterministic");
    }

    #[test]
    fn empty_text_embeds_to_zero_vector() {
        let adapter = NullAdapter::with_dim(8);
        let vectors = adapter.embed(&["".to_string()]).expect("embed succeeds");
        assert_eq!(vectors[0], vec![0.0f32; 8]);
    }

    #[test]
    fn prompt_token_estimate_rounds_up() {
        assert_eq!(prompt_token_estimate(""), 0);
        assert_eq!(prompt_token_estimate("abcd"), 1);
        assert_eq!(prompt_token_estimate("abcde"), 2);
    }
}
