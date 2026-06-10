//! Provider adapter seam plus the two offline adapters: the deterministic `null`
//! hash adapter (no semantic signal, CI/fallback) and the in-process `local`
//! adapter (bge-small via tract, real semantic signal, no network, no spend).
//! Remote `openai_compat`/`ollama`/`opencode` adapters land later behind this
//! same trait.

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
    /// Embed a recall query. Defaults to `embed`; asymmetric-retrieval models
    /// (bge) override this to apply their query instruction prefix.
    fn embed_query(&self, text: &str) -> Result<Vec<f32>, AdapterError> {
        let mut vectors = self.embed(std::slice::from_ref(&text.to_string()))?;
        vectors
            .pop()
            .ok_or_else(|| AdapterError::Embed("adapter returned no vector".to_string()))
    }
    /// Whether the adapter can currently serve requests.
    fn reachable(&self) -> bool;
    /// Whether this adapter's embeddings carry a usable semantic signal. The
    /// `null` adapter's hash vectors do not, so recall must not rerank by them.
    fn embeds_semantically(&self) -> bool {
        true
    }
    /// Summarize a cluster of texts into one consolidated memory body. `None` means
    /// the adapter has no LLM/chat capability (e.g. `null`), so the caller falls back
    /// to a deterministic lexical representative. The default keeps consolidation
    /// network-free and free of spend.
    fn summarize(&self, _texts: &[String]) -> Result<Option<String>, AdapterError> {
        Ok(None)
    }
    /// Price signal used to gate LLM summarization against the dream spend cap. `0.0`
    /// (the default, and `null`) means free, so the spend cap never binds.
    fn usd_per_1k_prompt_tokens(&self) -> f64 {
        0.0
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

/// In-process semantic embedding adapter: bge-small-en-v1.5 via tract (see
/// `localembed`). Deterministic, offline, free — and unlike `null`, its vectors
/// carry a real semantic signal, so recall reranks by them.
#[derive(Debug, Clone, Copy, Default)]
pub struct LocalAdapter;

impl ProviderAdapter for LocalAdapter {
    fn id(&self) -> &'static str {
        "local"
    }

    fn model_id(&self) -> &str {
        crate::localembed::MODEL_ID
    }

    fn reachable(&self) -> bool {
        true
    }

    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, AdapterError> {
        texts
            .iter()
            .map(|text| crate::localembed::embed_passage(text).map_err(AdapterError::Embed))
            .collect()
    }

    fn embed_query(&self, text: &str) -> Result<Vec<f32>, AdapterError> {
        crate::localembed::embed_query(text).map_err(AdapterError::Embed)
    }
}

/// Runtime-selected adapter (`Config.providers.default_adapter`). An enum rather
/// than a trait object so the generic `A: ProviderAdapter` plumbing through
/// store/worker/dream stays unchanged.
#[derive(Debug, Clone)]
pub enum AdapterKind {
    Null(NullAdapter),
    Local(LocalAdapter),
}

impl AdapterKind {
    /// Resolve the configured default adapter. Unknown names fall back to `null`
    /// (config validation rejects them before this point; the fallback keeps this
    /// constructor total). Remote adapters are not built yet and also resolve to
    /// `null`, degrading exactly like the pre-local behavior.
    pub fn from_default_adapter(name: &str) -> Self {
        match name {
            "local" => Self::Local(LocalAdapter),
            _ => Self::Null(NullAdapter::new()),
        }
    }
}

impl ProviderAdapter for AdapterKind {
    fn id(&self) -> &'static str {
        match self {
            Self::Null(a) => a.id(),
            Self::Local(a) => a.id(),
        }
    }

    fn model_id(&self) -> &str {
        match self {
            Self::Null(a) => a.model_id(),
            Self::Local(a) => a.model_id(),
        }
    }

    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, AdapterError> {
        match self {
            Self::Null(a) => a.embed(texts),
            Self::Local(a) => a.embed(texts),
        }
    }

    fn embed_query(&self, text: &str) -> Result<Vec<f32>, AdapterError> {
        match self {
            Self::Null(a) => a.embed_query(text),
            Self::Local(a) => a.embed_query(text),
        }
    }

    fn reachable(&self) -> bool {
        match self {
            Self::Null(a) => a.reachable(),
            Self::Local(a) => a.reachable(),
        }
    }

    fn embeds_semantically(&self) -> bool {
        match self {
            Self::Null(a) => a.embeds_semantically(),
            Self::Local(a) => a.embeds_semantically(),
        }
    }

    fn summarize(&self, texts: &[String]) -> Result<Option<String>, AdapterError> {
        match self {
            Self::Null(a) => a.summarize(texts),
            Self::Local(a) => a.summarize(texts),
        }
    }

    fn usd_per_1k_prompt_tokens(&self) -> f64 {
        match self {
            Self::Null(a) => a.usd_per_1k_prompt_tokens(),
            Self::Local(a) => a.usd_per_1k_prompt_tokens(),
        }
    }
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
    fn local_adapter_surface_is_semantic_free_and_reachable() {
        let adapter = LocalAdapter;
        assert_eq!(adapter.id(), "local");
        assert_eq!(adapter.model_id(), crate::localembed::MODEL_ID);
        assert!(adapter.reachable());
        assert!(adapter.embeds_semantically());
        assert_eq!(adapter.usd_per_1k_prompt_tokens(), 0.0);
        assert!(adapter.summarize(&[]).expect("no llm").is_none());
    }

    #[test]
    fn adapter_kind_resolves_local_and_falls_back_to_null() {
        assert!(matches!(
            AdapterKind::from_default_adapter("local"),
            AdapterKind::Local(_)
        ));
        assert!(matches!(
            AdapterKind::from_default_adapter("null"),
            AdapterKind::Null(_)
        ));
        let fallback = AdapterKind::from_default_adapter("ollama");
        assert!(matches!(fallback, AdapterKind::Null(_)));
        assert!(!fallback.embeds_semantically());
    }

    #[test]
    fn default_embed_query_matches_embed_for_symmetric_adapters() {
        let adapter = NullAdapter::new();
        let via_embed = adapter.embed(&["note".to_string()]).expect("ok");
        let via_query = adapter.embed_query("note").expect("ok");
        assert_eq!(via_embed[0], via_query);
    }

    #[test]
    fn prompt_token_estimate_rounds_up() {
        assert_eq!(prompt_token_estimate(""), 0);
        assert_eq!(prompt_token_estimate("abcd"), 1);
        assert_eq!(prompt_token_estimate("abcde"), 2);
    }
}
