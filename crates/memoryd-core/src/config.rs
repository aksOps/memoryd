use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;

pub const DEFAULT_BIND: &str = "127.0.0.1:7077";
/// Minimum bearer-token length accepted for a non-loopback bind. The token is
/// the entire security boundary for remote callers, so trivially guessable
/// values are rejected at startup; loopback dev tokens stay unrestricted.
pub const MIN_BEARER_TOKEN_LEN: usize = 16;
/// Upper bound (24h) on duration caps. Larger values used to saturate the
/// downstream millisecond conversions to i64::MAX — silently meaning "no cap
/// at all" — so they are rejected at validation instead.
pub const MAX_DURATION_SECS: u64 = 86_400;
/// Upper bound (10 years) on retention horizons; 0 disables retention.
pub const MAX_RETAIN_DAYS: u64 = 3_650;

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub bind: SocketAddr,
    pub bearer_token: Option<String>,
    pub db_path: PathBuf,
    pub caps: Caps,
    pub providers: ProviderConfig,
}

impl Config {
    pub fn with_db_path(db_path: PathBuf) -> Self {
        Self {
            bind: DEFAULT_BIND
                .parse()
                .expect("DEFAULT_BIND must be a valid socket address"),
            bearer_token: None,
            db_path,
            caps: Caps::small(),
            providers: ProviderConfig::default(),
        }
    }

    /// Apply provider settings from process environment variables:
    /// `MEMORYD_ADAPTER` (null|local|openai_compat), `MEMORYD_EMBED_ADAPTER`
    /// (same set; embed-only override), `MEMORYD_AUTO_APPROVE` (auto-accept
    /// dream-proposed profile facts), `MEMORYD_SPEND_CAP_USD`, and the
    /// `MEMORYD_OPENAI_*` family (BASE_URL, API_KEY, API_KEY_FILE, EMBED_MODEL,
    /// CHAT_MODEL, USD_PER_1K). Call before [`Config::validate`].
    pub fn apply_env(&mut self) -> Result<(), ConfigError> {
        self.apply_env_from(&|name| std::env::var(name).ok())
    }

    /// Env application with an injected lookup so tests avoid process-global
    /// environment mutation.
    pub fn apply_env_from(
        &mut self,
        get: &dyn Fn(&str) -> Option<String>,
    ) -> Result<(), ConfigError> {
        if let Some(adapter) = get("MEMORYD_ADAPTER") {
            self.providers.default_adapter = adapter;
        }
        if let Some(embed_adapter) = get("MEMORYD_EMBED_ADAPTER") {
            self.providers.embed_adapter = Some(embed_adapter);
        }
        if let Some(value) = get("MEMORYD_AUTO_APPROVE") {
            self.caps.auto_approve_profile_facts = parse_bool_env(&value);
        }
        if let Some(cap) = get("MEMORYD_SPEND_CAP_USD") {
            let cap = cap
                .parse::<f64>()
                .ok()
                .filter(|value| value.is_finite() && *value >= 0.0)
                .ok_or(ConfigError::InvalidNumberEnv {
                    var: "MEMORYD_SPEND_CAP_USD",
                })?;
            self.providers.paid_spend_cap_usd = cap;
            self.caps.paid_spend_cap_usd = cap;
        }
        if let Some(base_url) = get("MEMORYD_OPENAI_BASE_URL") {
            self.providers.openai_compat.base_url = base_url.trim_end_matches('/').to_string();
        }
        // API_KEY_FILE wins over API_KEY (same hygiene rationale as
        // --token-file: files can be chmod 0600, environments leak more ways).
        if let Some(key) = get("MEMORYD_OPENAI_API_KEY") {
            self.providers.openai_compat.api_key = Some(key);
        }
        if let Some(path) = get("MEMORYD_OPENAI_API_KEY_FILE") {
            let contents = std::fs::read_to_string(&path)
                .map_err(|_| ConfigError::ApiKeyFileUnreadable { path })?;
            self.providers.openai_compat.api_key =
                Some(contents.trim_end_matches(['\r', '\n']).to_string());
        }
        for (var, target) in [
            ("MEMORYD_RETAIN_RAW_DAYS", 0usize),
            ("MEMORYD_RETAIN_RAW_EMBED_DAYS", 1usize),
        ] {
            if let Some(days) = get(var) {
                let days = days
                    .parse::<u64>()
                    .ok()
                    .filter(|value| *value <= MAX_RETAIN_DAYS)
                    .ok_or(ConfigError::InvalidNumberEnv {
                        var: if target == 0 {
                            "MEMORYD_RETAIN_RAW_DAYS"
                        } else {
                            "MEMORYD_RETAIN_RAW_EMBED_DAYS"
                        },
                    })?;
                if target == 0 {
                    self.caps.retain_raw_days = days;
                } else {
                    self.caps.retain_raw_embeddings_days = days;
                }
            }
        }
        if let Some(model) = get("MEMORYD_OPENAI_EMBED_MODEL") {
            self.providers.openai_compat.embed_model = model;
        }
        if let Some(model) = get("MEMORYD_OPENAI_CHAT_MODEL") {
            self.providers.openai_compat.chat_model = model;
        }
        if let Some(price) = get("MEMORYD_OPENAI_USD_PER_1K") {
            self.providers.openai_compat.usd_per_1k_prompt_tokens = price
                .parse::<f64>()
                .ok()
                .filter(|value| value.is_finite() && *value >= 0.0)
                .ok_or(ConfigError::InvalidNumberEnv {
                    var: "MEMORYD_OPENAI_USD_PER_1K",
                })?;
        }
        Ok(())
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if !is_loopback(self.bind.ip()) && self.bearer_token.is_none() {
            return Err(ConfigError::RemoteBindRequiresBearer { bind: self.bind });
        }

        if let Some(token) = self.bearer_token.as_deref() {
            // An empty token can never match any Authorization header, so it
            // fails closed — but a clear startup error beats a silent self-DoS
            // (e.g. MEMORYD_TOKEN set-but-empty in a shell script).
            if token.trim().is_empty() {
                return Err(ConfigError::EmptyBearerToken);
            }
            if !is_loopback(self.bind.ip()) && token.len() < MIN_BEARER_TOKEN_LEN {
                return Err(ConfigError::BearerTokenTooShort { len: token.len() });
            }
        }

        // One generic remote adapter, not provider-specific names: Ollama,
        // vLLM, LM Studio, etc. are reached via `openai_compat` + base_url.
        if !matches!(
            self.providers.default_adapter.as_str(),
            "null" | "local" | "openai_compat"
        ) {
            return Err(ConfigError::UnknownAdapter {
                adapter: self.providers.default_adapter.clone(),
            });
        }

        // The embed-only override draws from the same adapter set.
        if let Some(embed_adapter) = self.providers.embed_adapter.as_deref()
            && !matches!(embed_adapter, "null" | "local" | "openai_compat")
        {
            return Err(ConfigError::UnknownAdapter {
                adapter: embed_adapter.to_string(),
            });
        }

        if self.providers.default_adapter == "openai_compat" {
            let base = self.providers.openai_compat.base_url.as_str();
            if !(base.starts_with("http://") || base.starts_with("https://")) {
                return Err(ConfigError::InvalidBaseUrl {
                    base_url: base.to_string(),
                });
            }
            if self
                .providers
                .openai_compat
                .api_key
                .as_deref()
                .is_some_and(|key| key.trim().is_empty())
            {
                return Err(ConfigError::EmptyApiKey);
            }
        }

        // 'null' and 'local' run in-process and spend nothing; only remote adapters
        // need a non-zero paid budget (H5).
        if !matches!(self.providers.default_adapter.as_str(), "null" | "local")
            && self.providers.paid_spend_cap_usd == 0.0
        {
            return Err(ConfigError::PaidProviderRequiresBudget {
                adapter: self.providers.default_adapter.clone(),
            });
        }

        if !matches!(self.caps.vector_index_kind.as_str(), "brute-force" | "hnsw") {
            return Err(ConfigError::UnknownVectorIndex {
                kind: self.caps.vector_index_kind.clone(),
            });
        }

        for (field, value) in [
            ("dream_wallclock_secs", self.caps.dream_wallclock_secs),
            ("lease_visibility_secs", self.caps.lease_visibility_secs),
        ] {
            if value > MAX_DURATION_SECS {
                return Err(ConfigError::CapDurationTooLarge {
                    field,
                    value,
                    max: MAX_DURATION_SECS,
                });
            }
        }

        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Caps {
    pub queue_depth_max: usize,
    pub worker_concurrency: usize,
    pub worker_mem_mb: usize,
    pub dream_wallclock_secs: u64,
    pub paid_spend_cap_usd: f64,
    pub lease_visibility_secs: u64,
    pub job_max_attempts: u32,
    pub job_backoff_base_ms: u64,
    /// Retention horizon (days) for consolidated raw events; 0 = keep forever
    /// (the default — deleting history is an explicit owner opt-in via
    /// `MEMORYD_RETAIN_RAW_DAYS`). Memories, the graph, and audit stay.
    pub retain_raw_days: u64,
    /// Retention horizon (days) for raw-event embeddings (the second-largest
    /// growth component; memory-level embeddings remain). 0 = keep forever.
    pub retain_raw_embeddings_days: u64,
    /// Which `VectorIndex` implementation recall uses: "brute-force" (default, oracle)
    /// or "hnsw" (ARCHITECTURE-PLAN §21.12).
    pub vector_index_kind: String,
    /// Auto-accept dream-proposed profile facts without manual `approve`. On by
    /// default for a hands-off profile; set `MEMORYD_AUTO_APPROVE=false` to
    /// restore the manual human-review gate (H6). Scoped to `profile_fact`
    /// approvals only — deletions (`memory_cleanup`/`memory_purge`) are never
    /// auto-approved regardless of this setting.
    pub auto_approve_profile_facts: bool,
}

impl Caps {
    pub fn small() -> Self {
        Self {
            queue_depth_max: 10_000,
            worker_concurrency: 1,
            worker_mem_mb: 128,
            dream_wallclock_secs: 180,
            paid_spend_cap_usd: 0.0,
            lease_visibility_secs: 30,
            job_max_attempts: 5,
            job_backoff_base_ms: 500,
            retain_raw_days: 0,
            retain_raw_embeddings_days: 0,
            vector_index_kind: "brute-force".to_string(),
            auto_approve_profile_facts: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProviderConfig {
    pub default_adapter: String,
    /// Optional override for embedding operations only (recall query embeds,
    /// the serve embed worker, and the dream associate phase). When `None`,
    /// embeds default to `local` (or `null` when `default_adapter` is `null`)
    /// — see [`ProviderConfig::effective_embed_adapter`]. This keeps embeddings
    /// on the in-process bge-small vectors while `default_adapter` drives the
    /// LLM/chat phases via a remote provider — the embed/chat split that lets a
    /// paid chat model fill the profile without re-embedding the corpus or
    /// routing recall through a remote endpoint. Set explicitly (e.g.
    /// `openai_compat`) only to opt back into remote embeddings.
    pub embed_adapter: Option<String>,
    pub paid_spend_cap_usd: f64,
    pub openai_compat: OpenAiCompatConfig,
}

impl ProviderConfig {
    /// The adapter name governing embedding operations. The `embed_adapter`
    /// override wins if set; otherwise embeddings default to the in-process
    /// `local` model so configuring a remote chat provider never re-routes
    /// recall or persisted vectors to the cloud. The one exception is the
    /// `null` adapter (CI/deterministic, no model load), which stays `null`.
    pub fn effective_embed_adapter(&self) -> &str {
        if let Some(embed) = self.embed_adapter.as_deref() {
            return embed;
        }
        if self.default_adapter == "null" {
            "null"
        } else {
            "local"
        }
    }
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            default_adapter: "local".to_string(),
            embed_adapter: None,
            paid_spend_cap_usd: 0.0,
            openai_compat: OpenAiCompatConfig::default(),
        }
    }
}

/// Settings for the generic OpenAI-compatible adapter. Provider-agnostic by
/// design: any endpoint speaking the OpenAI embeddings/chat-completions wire
/// shape works (api.openai.com, Ollama's `/v1`, vLLM, LM Studio, llama.cpp).
#[derive(Debug, Clone, PartialEq)]
pub struct OpenAiCompatConfig {
    /// Base URL up to and including the API root, e.g.
    /// `https://api.openai.com/v1` or `http://127.0.0.1:11434/v1`.
    pub base_url: String,
    /// Bearer key sent as `Authorization: Bearer <key>`. `None` for local
    /// runtimes that do not authenticate.
    pub api_key: Option<String>,
    /// Model for `POST {base}/embeddings`.
    pub embed_model: String,
    /// Model for `POST {base}/chat/completions` (dream summarization).
    pub chat_model: String,
    /// Price signal for the dream spend governor; 0.0 = free (local runtimes).
    pub usd_per_1k_prompt_tokens: f64,
}

impl Default for OpenAiCompatConfig {
    fn default() -> Self {
        Self {
            base_url: "https://api.openai.com/v1".to_string(),
            api_key: None,
            embed_model: "text-embedding-3-small".to_string(),
            chat_model: "gpt-4o-mini".to_string(),
            usd_per_1k_prompt_tokens: 0.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ConfigError {
    RemoteBindRequiresBearer {
        bind: SocketAddr,
    },
    EmptyBearerToken,
    BearerTokenTooShort {
        len: usize,
    },
    CapDurationTooLarge {
        field: &'static str,
        value: u64,
        max: u64,
    },
    InvalidBaseUrl {
        base_url: String,
    },
    EmptyApiKey,
    ApiKeyFileUnreadable {
        path: String,
    },
    InvalidNumberEnv {
        var: &'static str,
    },
    PaidProviderRequiresBudget {
        adapter: String,
    },
    UnknownVectorIndex {
        kind: String,
    },
    UnknownAdapter {
        adapter: String,
    },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RemoteBindRequiresBearer { bind } => {
                write!(f, "non-loopback bind {bind} requires a bearer token")
            }
            Self::EmptyBearerToken => {
                write!(
                    f,
                    "bearer token must not be empty (set MEMORYD_TOKEN or --token)"
                )
            }
            Self::BearerTokenTooShort { len } => {
                write!(
                    f,
                    "bearer token must be at least {MIN_BEARER_TOKEN_LEN} characters for a non-loopback bind (got {len})"
                )
            }
            Self::CapDurationTooLarge { field, value, max } => {
                write!(f, "{field} must be at most {max} seconds (got {value})")
            }
            Self::InvalidBaseUrl { base_url } => {
                write!(
                    f,
                    "openai_compat base URL must start with http:// or https:// (got {base_url})"
                )
            }
            Self::EmptyApiKey => {
                write!(
                    f,
                    "openai_compat API key must not be empty (unset it for keyless local runtimes)"
                )
            }
            Self::ApiKeyFileUnreadable { path } => {
                write!(f, "could not read API key file {path}")
            }
            Self::InvalidNumberEnv { var } => {
                write!(f, "{var} must be a non-negative number")
            }
            Self::PaidProviderRequiresBudget { adapter } => write!(
                f,
                "provider adapter {adapter} is not allowed with a zero paid-spend budget"
            ),
            Self::UnknownVectorIndex { kind } => {
                write!(
                    f,
                    "unknown vector index kind: {kind} (expected brute-force or hnsw)"
                )
            }
            Self::UnknownAdapter { adapter } => {
                write!(f, "unknown provider adapter: {adapter}")
            }
        }
    }
}

impl std::error::Error for ConfigError {}

fn is_loopback(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => v6.is_loopback(),
    }
}

/// Lenient truthy parse for boolean env vars: `1`/`true`/`yes`/`on`
/// (case-insensitive) enable; anything else (including unset upstream) is false.
fn parse_bool_env(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_localhost_local_provider_zero_spend() {
        let cfg = Config::with_db_path(PathBuf::from("memoryd.db"));

        assert_eq!(cfg.bind.to_string(), DEFAULT_BIND);
        assert_eq!(cfg.providers.default_adapter, "local");
        assert_eq!(cfg.caps.worker_concurrency, 1);
        assert_eq!(cfg.caps.paid_spend_cap_usd, 0.0);
        assert_eq!(cfg.caps.lease_visibility_secs, 30);
        assert_eq!(cfg.caps.job_max_attempts, 5);
        assert_eq!(cfg.caps.job_backoff_base_ms, 500);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn non_loopback_bind_requires_bearer_token() {
        let mut cfg = Config::with_db_path(PathBuf::from("memoryd.db"));
        cfg.bind = "0.0.0.0:7077".parse().expect("test bind parses");

        assert!(matches!(
            cfg.validate(),
            Err(ConfigError::RemoteBindRequiresBearer { .. })
        ));

        cfg.bearer_token = Some("test-token-0123456789".to_string());
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn validate_rejects_empty_bearer_token() {
        let mut cfg = Config::with_db_path(PathBuf::from("memoryd.db"));
        for empty in ["", "   "] {
            cfg.bearer_token = Some(empty.to_string());
            assert!(
                matches!(cfg.validate(), Err(ConfigError::EmptyBearerToken)),
                "empty token rejected on loopback"
            );
        }
        cfg.bind = "0.0.0.0:7077".parse().expect("test bind parses");
        cfg.bearer_token = Some(String::new());
        assert!(
            matches!(cfg.validate(), Err(ConfigError::EmptyBearerToken)),
            "empty token rejected on non-loopback"
        );
    }

    #[test]
    fn validate_rejects_short_token_for_non_loopback() {
        let mut cfg = Config::with_db_path(PathBuf::from("memoryd.db"));
        cfg.bind = "0.0.0.0:7077".parse().expect("test bind parses");
        cfg.bearer_token = Some("fifteen-chars-x".to_string());
        assert!(matches!(
            cfg.validate(),
            Err(ConfigError::BearerTokenTooShort { len: 15 })
        ));
    }

    #[test]
    fn validate_allows_short_token_on_loopback() {
        let mut cfg = Config::with_db_path(PathBuf::from("memoryd.db"));
        cfg.bearer_token = Some("dev".to_string());
        assert!(cfg.validate().is_ok(), "loopback dev tokens stay legal");
    }

    #[test]
    fn validate_rejects_oversized_dream_wallclock() {
        let mut cfg = Config::with_db_path(PathBuf::from("memoryd.db"));
        cfg.caps.dream_wallclock_secs = MAX_DURATION_SECS + 1;
        assert!(matches!(
            cfg.validate(),
            Err(ConfigError::CapDurationTooLarge {
                field: "dream_wallclock_secs",
                ..
            })
        ));
    }

    #[test]
    fn validate_rejects_oversized_lease_visibility() {
        let mut cfg = Config::with_db_path(PathBuf::from("memoryd.db"));
        cfg.caps.lease_visibility_secs = MAX_DURATION_SECS + 1;
        assert!(matches!(
            cfg.validate(),
            Err(ConfigError::CapDurationTooLarge {
                field: "lease_visibility_secs",
                ..
            })
        ));
    }

    #[test]
    fn validate_accepts_durations_at_bound() {
        let mut cfg = Config::with_db_path(PathBuf::from("memoryd.db"));
        cfg.caps.dream_wallclock_secs = MAX_DURATION_SECS;
        cfg.caps.lease_visibility_secs = MAX_DURATION_SECS;
        assert!(cfg.validate().is_ok(), "exactly 24h validates");
    }

    #[test]
    fn validate_adapter_rules() {
        let mut cfg = Config::with_db_path(PathBuf::from("memoryd.db"));
        for free in ["null", "local"] {
            cfg.providers.default_adapter = free.to_string();
            cfg.providers.paid_spend_cap_usd = 0.0;
            assert!(cfg.validate().is_ok(), "{free} is free, no budget needed");
        }
        cfg.providers.default_adapter = "openai_compat".to_string();
        assert!(matches!(
            cfg.validate(),
            Err(ConfigError::PaidProviderRequiresBudget { .. })
        ));
        cfg.providers.paid_spend_cap_usd = 1.0;
        assert!(
            cfg.validate().is_ok(),
            "remote adapter with budget validates"
        );
        for provider_specific in ["ollama", "opencode", "bogus"] {
            cfg.providers.default_adapter = provider_specific.to_string();
            assert!(
                matches!(cfg.validate(), Err(ConfigError::UnknownAdapter { .. })),
                "provider-specific adapter names are gone; use openai_compat + base_url"
            );
        }
    }

    #[test]
    fn embed_adapter_override_resolves_and_validates() {
        let mut cfg = Config::with_db_path(PathBuf::from("memoryd.db"));
        // Default (local provider): embeds are local.
        assert_eq!(cfg.providers.effective_embed_adapter(), "local");

        // Remote chat provider, no override: embeds still default to local —
        // the split is the default so the cloud never gets the embeddings.
        cfg.providers.default_adapter = "openai_compat".to_string();
        cfg.providers.paid_spend_cap_usd = 1.0;
        cfg.providers.openai_compat.base_url = "http://127.0.0.1:11434/v1".to_string();
        assert_eq!(cfg.providers.effective_embed_adapter(), "local");

        // The null adapter (CI/deterministic) keeps null embeddings, no override.
        let mut null_cfg = Config::with_db_path(PathBuf::from("memoryd.db"));
        null_cfg.providers.default_adapter = "null".to_string();
        assert_eq!(null_cfg.providers.effective_embed_adapter(), "null");

        // Explicit override is still honored and validated.
        cfg.providers.embed_adapter = Some("local".to_string());
        assert_eq!(cfg.providers.effective_embed_adapter(), "local");
        assert!(
            cfg.validate().is_ok(),
            "openai_compat chat + local embed override validates"
        );

        // An unknown embed adapter is rejected like any unknown adapter.
        cfg.providers.embed_adapter = Some("bogus".to_string());
        assert!(matches!(
            cfg.validate(),
            Err(ConfigError::UnknownAdapter { adapter }) if adapter == "bogus"
        ));
    }

    #[test]
    fn apply_env_reads_embed_adapter_override() {
        let mut cfg = Config::with_db_path(PathBuf::from("memoryd.db"));
        cfg.apply_env_from(&|name| (name == "MEMORYD_EMBED_ADAPTER").then(|| "local".to_string()))
            .expect("env applies");
        assert_eq!(cfg.providers.embed_adapter.as_deref(), Some("local"));
        assert_eq!(cfg.providers.effective_embed_adapter(), "local");
    }

    #[test]
    fn auto_approve_defaults_on_and_env_can_disable() {
        let mut cfg = Config::with_db_path(PathBuf::from("memoryd.db"));
        assert!(
            cfg.caps.auto_approve_profile_facts,
            "hands-off profile is the default"
        );
        cfg.apply_env_from(&|name| (name == "MEMORYD_AUTO_APPROVE").then(|| "false".to_string()))
            .expect("env applies");
        assert!(
            !cfg.caps.auto_approve_profile_facts,
            "env restores the manual gate"
        );
    }

    #[test]
    fn parse_bool_env_is_lenient_and_defaults_false() {
        for truthy in ["1", "true", "TRUE", " yes ", "on"] {
            assert!(parse_bool_env(truthy), "{truthy:?} should be truthy");
        }
        for falsy in ["0", "false", "no", "", "off", "maybe"] {
            assert!(!parse_bool_env(falsy), "{falsy:?} should be falsy");
        }
    }

    #[test]
    fn validate_openai_compat_rejects_bad_base_url_and_empty_key() {
        let mut cfg = Config::with_db_path(PathBuf::from("memoryd.db"));
        cfg.providers.default_adapter = "openai_compat".to_string();
        cfg.providers.paid_spend_cap_usd = 1.0;

        cfg.providers.openai_compat.base_url = "ftp://example".to_string();
        assert!(matches!(
            cfg.validate(),
            Err(ConfigError::InvalidBaseUrl { .. })
        ));

        cfg.providers.openai_compat.base_url = "http://127.0.0.1:11434/v1".to_string();
        cfg.providers.openai_compat.api_key = Some("  ".to_string());
        assert!(matches!(cfg.validate(), Err(ConfigError::EmptyApiKey)));

        cfg.providers.openai_compat.api_key = None;
        assert!(cfg.validate().is_ok(), "keyless local runtime validates");
    }

    #[test]
    fn apply_env_overrides_provider_settings() {
        let mut cfg = Config::with_db_path(PathBuf::from("memoryd.db"));
        let env = |name: &str| -> Option<String> {
            match name {
                "MEMORYD_ADAPTER" => Some("openai_compat".to_string()),
                "MEMORYD_SPEND_CAP_USD" => Some("2.5".to_string()),
                "MEMORYD_OPENAI_BASE_URL" => Some("http://127.0.0.1:11434/v1/".to_string()),
                "MEMORYD_OPENAI_API_KEY" => Some("env-key".to_string()),
                "MEMORYD_OPENAI_EMBED_MODEL" => Some("nomic-embed-text".to_string()),
                "MEMORYD_OPENAI_CHAT_MODEL" => Some("llama3.2".to_string()),
                "MEMORYD_OPENAI_USD_PER_1K" => Some("0.0001".to_string()),
                _ => None,
            }
        };
        cfg.apply_env_from(&env).expect("env applies");

        assert_eq!(cfg.providers.default_adapter, "openai_compat");
        assert_eq!(cfg.providers.paid_spend_cap_usd, 2.5);
        assert_eq!(cfg.caps.paid_spend_cap_usd, 2.5);
        assert_eq!(
            cfg.providers.openai_compat.base_url, "http://127.0.0.1:11434/v1",
            "trailing slash trimmed"
        );
        assert_eq!(
            cfg.providers.openai_compat.api_key.as_deref(),
            Some("env-key")
        );
        assert_eq!(cfg.providers.openai_compat.embed_model, "nomic-embed-text");
        assert_eq!(cfg.providers.openai_compat.chat_model, "llama3.2");
        assert_eq!(cfg.providers.openai_compat.usd_per_1k_prompt_tokens, 0.0001);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn apply_env_rejects_bad_numbers_and_prefers_key_file() {
        let mut cfg = Config::with_db_path(PathBuf::from("memoryd.db"));
        assert!(matches!(
            cfg.apply_env_from(&|name| (name == "MEMORYD_SPEND_CAP_USD").then(|| "-1".to_string())),
            Err(ConfigError::InvalidNumberEnv {
                var: "MEMORYD_SPEND_CAP_USD"
            })
        ));

        let key_path = std::env::temp_dir().join(format!(
            "memoryd-test-key-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::write(&key_path, "file-key\n").expect("key file written");
        let key_file = key_path.to_str().expect("utf-8 path").to_string();
        cfg.apply_env_from(&move |name| match name {
            "MEMORYD_OPENAI_API_KEY" => Some("env-key".to_string()),
            "MEMORYD_OPENAI_API_KEY_FILE" => Some(key_file.clone()),
            _ => None,
        })
        .expect("env applies");
        assert_eq!(
            cfg.providers.openai_compat.api_key.as_deref(),
            Some("file-key"),
            "key file wins over env key; newline trimmed"
        );
        let _ = std::fs::remove_file(&key_path);
    }

    #[test]
    fn validate_rejects_unknown_vector_index_kind() {
        let mut cfg = Config::with_db_path(std::path::PathBuf::from("/tmp/x.db"));
        assert!(cfg.validate().is_ok(), "default brute-force validates");
        cfg.caps.vector_index_kind = "hnsw".to_string();
        assert!(cfg.validate().is_ok(), "hnsw validates");
        cfg.caps.vector_index_kind = "bogus".to_string();
        assert!(matches!(
            cfg.validate(),
            Err(ConfigError::UnknownVectorIndex { .. })
        ));
    }
}
