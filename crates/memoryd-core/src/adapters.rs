//! Provider adapter seam plus the two offline adapters: the deterministic `null`
//! hash adapter (no semantic signal, CI/fallback) and the in-process `local`
//! adapter (bge-small via tract, real semantic signal, no network, no spend).
//! The remote seam is the single generic `openai_compat` adapter
//! (`crate::openaicompat`) — any OpenAI-shaped endpoint via base URL, not
//! provider-specific adapters.

use std::collections::hash_map::DefaultHasher;
use std::fmt;
use std::hash::{Hash, Hasher};

/// A provider behind the worker plane. Every adapter is entitlement-/budget-gated
/// at the call site; the seam itself only describes capabilities.
pub trait ProviderAdapter {
    /// Stable adapter identifier, matching the `provider_usage.adapter` enum.
    fn id(&self) -> &'static str;
    /// Model identifier recorded with each provider-usage row.
    fn model_id(&self) -> &str;
    /// Model identifier stamped on persisted embeddings (the `embeddings.model_id`
    /// column recall matches against). Defaults to [`ProviderAdapter::model_id`];
    /// a split embed/chat adapter overrides this so embeddings keep the embed
    /// model's id while LLM usage rows keep the chat model's id.
    fn embed_model_id(&self) -> &str {
        self.model_id()
    }
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
    /// Distill one work session's memories into a short narrative (what was done,
    /// decided, and why). Defaults to [`ProviderAdapter::summarize`]; chat-capable
    /// adapters override with a narrative-shaped prompt. `None` = no LLM, so the
    /// distill phase skips (sessions stay open for a later configured provider).
    fn distill(&self, texts: &[String]) -> Result<Option<String>, AdapterError> {
        self.summarize(texts)
    }
    /// Induce up to a few recurring, field-agnostic decision principles from a
    /// window of decisions and session narratives (one principle per output
    /// line). Strictly LLM-only — pattern induction across episodes has no
    /// deterministic fallback, so the default is `None` and the heuristic
    /// phase is inert without a chat-capable adapter.
    fn induce_heuristics(&self, _texts: &[String]) -> Result<Option<String>, AdapterError> {
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
    OpenAiCompat(crate::openaicompat::OpenAiCompatAdapter),
    /// Embed/chat split: embedding operations route to `embed`, LLM operations
    /// (summarize/distill/induce) to `chat`. Lets a remote chat provider drive
    /// the dream LLM phases while embeddings stay on a local model — keeping
    /// recall and persisted vectors on one consistent embed model.
    Split {
        embed: Box<AdapterKind>,
        chat: Box<AdapterKind>,
    },
}

impl AdapterKind {
    /// Resolve the configured provider. `openai_compat` carries endpoint
    /// settings, so callers with a full [`crate::config::ProviderConfig`]
    /// should use this; unknown names degrade to `null` (lexical-only recall)
    /// with a stderr warning so the degradation is never silent — though
    /// `Config::validate` rejects them before any long-lived caller gets here.
    pub fn from_provider_config(providers: &crate::config::ProviderConfig) -> Self {
        Self::resolve(providers, &providers.default_adapter)
    }

    /// Resolve the adapter governing embedding operations (recall query embeds,
    /// the embed worker, the dream associate phase): the `embed_adapter`
    /// override if set, else `default_adapter`. See
    /// [`crate::config::ProviderConfig::effective_embed_adapter`].
    pub fn embed_from_provider_config(providers: &crate::config::ProviderConfig) -> Self {
        Self::resolve(providers, providers.effective_embed_adapter())
    }

    /// Build the adapter a dream pass should run with: `chat` drives the LLM
    /// phases, `embed` the associate phase. Returns a [`AdapterKind::Split`]
    /// only when the two resolve to different adapters; otherwise the single
    /// shared adapter (so the common, un-split config keeps its exact behavior).
    /// `chat` should already be `effective_for_pass`-degraded by the caller.
    pub fn for_dream(embed: Self, chat: Self) -> Self {
        if embed.id() == chat.id() && embed.model_id() == chat.model_id() {
            return chat;
        }
        Self::Split {
            embed: Box::new(embed),
            chat: Box::new(chat),
        }
    }

    /// Resolve one adapter name against the provider settings. `openai_compat`
    /// carries endpoint settings; bare names go through [`Self::from_default_adapter`].
    fn resolve(providers: &crate::config::ProviderConfig, name: &str) -> Self {
        match name {
            "openai_compat" => Self::OpenAiCompat(
                crate::openaicompat::OpenAiCompatAdapter::from_config(&providers.openai_compat),
            ),
            other => Self::from_default_adapter(other),
        }
    }

    /// Failover for one governed pass (roadmap C1): remote adapters that fail
    /// their reachability probe degrade to the in-process `local` adapter for
    /// this pass — work proceeds with real (local) semantic vectors instead of
    /// burning retries against a dead endpoint. `null`/`local` return
    /// themselves untouched (always reachable, zero probe overhead).
    pub fn effective_for_pass(&self) -> (Self, bool) {
        match self {
            Self::OpenAiCompat(adapter) if !adapter.reachable() => {
                (Self::Local(LocalAdapter), true)
            }
            other => (other.clone(), false),
        }
    }

    /// Resolve by bare name (`null`/`local`). `openai_compat` requires endpoint
    /// settings and therefore [`AdapterKind::from_provider_config`]; resolving
    /// it by name alone degrades to `null` with a warning.
    pub fn from_default_adapter(name: &str) -> Self {
        match name {
            "local" => Self::Local(LocalAdapter),
            "null" => Self::Null(NullAdapter::new()),
            other => {
                eprintln!(
                    "memoryd: provider adapter '{other}' cannot be resolved by name alone; \
                     falling back to 'null' (lexical-only recall)"
                );
                Self::Null(NullAdapter::new())
            }
        }
    }
}

impl ProviderAdapter for AdapterKind {
    fn id(&self) -> &'static str {
        match self {
            Self::Null(a) => a.id(),
            Self::Local(a) => a.id(),
            Self::OpenAiCompat(a) => a.id(),
            // Usage rows attribute LLM ops, so the split adapter reports the chat id.
            Self::Split { chat, .. } => chat.id(),
        }
    }

    fn model_id(&self) -> &str {
        match self {
            Self::Null(a) => a.model_id(),
            Self::Local(a) => a.model_id(),
            Self::OpenAiCompat(a) => a.model_id(),
            Self::Split { chat, .. } => chat.model_id(),
        }
    }

    fn embed_model_id(&self) -> &str {
        match self {
            // Embeddings are stamped with the EMBED model so recall matches them.
            Self::Split { embed, .. } => embed.model_id(),
            other => other.model_id(),
        }
    }

    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, AdapterError> {
        match self {
            Self::Null(a) => a.embed(texts),
            Self::Local(a) => a.embed(texts),
            Self::OpenAiCompat(a) => a.embed(texts),
            Self::Split { embed, .. } => embed.embed(texts),
        }
    }

    fn embed_query(&self, text: &str) -> Result<Vec<f32>, AdapterError> {
        match self {
            Self::Null(a) => a.embed_query(text),
            Self::Local(a) => a.embed_query(text),
            Self::OpenAiCompat(a) => a.embed_query(text),
            Self::Split { embed, .. } => embed.embed_query(text),
        }
    }

    fn reachable(&self) -> bool {
        match self {
            Self::Null(a) => a.reachable(),
            Self::Local(a) => a.reachable(),
            Self::OpenAiCompat(a) => a.reachable(),
            // Gates the embed-driven associate phase, so the embed side decides.
            Self::Split { embed, .. } => embed.reachable(),
        }
    }

    fn embeds_semantically(&self) -> bool {
        match self {
            Self::Null(a) => a.embeds_semantically(),
            Self::Local(a) => a.embeds_semantically(),
            Self::OpenAiCompat(a) => a.embeds_semantically(),
            Self::Split { embed, .. } => embed.embeds_semantically(),
        }
    }

    fn summarize(&self, texts: &[String]) -> Result<Option<String>, AdapterError> {
        match self {
            Self::Null(a) => a.summarize(texts),
            Self::Local(a) => a.summarize(texts),
            Self::OpenAiCompat(a) => a.summarize(texts),
            Self::Split { chat, .. } => chat.summarize(texts),
        }
    }

    fn distill(&self, texts: &[String]) -> Result<Option<String>, AdapterError> {
        match self {
            Self::Null(a) => a.distill(texts),
            Self::Local(a) => a.distill(texts),
            Self::OpenAiCompat(a) => a.distill(texts),
            Self::Split { chat, .. } => chat.distill(texts),
        }
    }

    fn induce_heuristics(&self, texts: &[String]) -> Result<Option<String>, AdapterError> {
        match self {
            Self::Null(a) => a.induce_heuristics(texts),
            Self::Local(a) => a.induce_heuristics(texts),
            Self::OpenAiCompat(a) => a.induce_heuristics(texts),
            Self::Split { chat, .. } => chat.induce_heuristics(texts),
        }
    }

    fn usd_per_1k_prompt_tokens(&self) -> f64 {
        match self {
            Self::Null(a) => a.usd_per_1k_prompt_tokens(),
            Self::Local(a) => a.usd_per_1k_prompt_tokens(),
            Self::OpenAiCompat(a) => a.usd_per_1k_prompt_tokens(),
            Self::Split { chat, .. } => chat.usd_per_1k_prompt_tokens(),
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
    Summarize(String),
}

impl fmt::Display for AdapterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Embed(message) => write!(f, "embed failed: {message}"),
            Self::Summarize(message) => write!(f, "summarize failed: {message}"),
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

    fn openai_compat_providers() -> crate::config::ProviderConfig {
        let mut providers = crate::config::ProviderConfig {
            default_adapter: "openai_compat".to_string(),
            ..Default::default()
        };
        providers.openai_compat.base_url = "http://127.0.0.1:1/v1".to_string();
        providers
    }

    #[test]
    fn embed_from_provider_config_honors_the_override() {
        let mut providers = openai_compat_providers();
        providers.embed_adapter = Some("local".to_string());

        // Chat resolves to the remote provider; embeds resolve to local.
        assert_eq!(
            AdapterKind::from_provider_config(&providers).id(),
            "openai_compat"
        );
        assert_eq!(
            AdapterKind::embed_from_provider_config(&providers).id(),
            "local"
        );

        // Without an override, embeds follow the default adapter.
        providers.embed_adapter = None;
        assert_eq!(
            AdapterKind::embed_from_provider_config(&providers).id(),
            "openai_compat"
        );
    }

    #[test]
    fn for_dream_collapses_when_embed_and_chat_match() {
        let single = AdapterKind::for_dream(
            AdapterKind::from_default_adapter("local"),
            AdapterKind::from_default_adapter("local"),
        );
        assert!(
            !matches!(single, AdapterKind::Split { .. }),
            "same embed+chat stays a single adapter"
        );
        assert_eq!(single.id(), "local");
    }

    #[test]
    fn split_routes_embed_to_embed_and_llm_to_chat() {
        // embed = Local (semantic bge), chat = Null (no LLM).
        let split = AdapterKind::for_dream(
            AdapterKind::from_default_adapter("local"),
            AdapterKind::from_default_adapter("null"),
        );
        assert!(
            matches!(split, AdapterKind::Split { .. }),
            "differing sides split"
        );

        // Identity/usage reflect the CHAT side; embeddings the EMBED side.
        assert_eq!(split.id(), "null", "usage rows attribute the chat adapter");
        assert_eq!(split.model_id(), "null-hash-32");
        assert_eq!(
            split.embed_model_id(),
            crate::localembed::MODEL_ID,
            "persisted embeddings keep the embed model id"
        );

        // Embeds route to Local: 384-dim, semantic.
        let vectors = split
            .embed(&["lock contention".to_string()])
            .expect("embed");
        assert_eq!(vectors[0].len(), 384);
        assert!(split.embeds_semantically());
        assert!(split.reachable());

        // LLM ops route to the (null) chat side, exercising those arms.
        assert!(
            split
                .summarize(&["x".to_string()])
                .expect("no llm")
                .is_none()
        );
        assert!(split.distill(&["x".to_string()]).expect("no llm").is_none());
        assert!(
            split
                .induce_heuristics(&["x".to_string()])
                .expect("no llm")
                .is_none()
        );
        assert_eq!(split.usd_per_1k_prompt_tokens(), 0.0);
    }

    #[test]
    fn split_embed_model_id_follows_the_embed_side() {
        // Reverse the roles: embed = Null, chat = Local.
        let split = AdapterKind::for_dream(
            AdapterKind::from_default_adapter("null"),
            AdapterKind::from_default_adapter("local"),
        );
        assert_eq!(
            split.embed_model_id(),
            "null-hash-32",
            "embed side stamps vectors"
        );
        assert_eq!(
            split.model_id(),
            crate::localembed::MODEL_ID,
            "chat side for usage"
        );
    }
}
