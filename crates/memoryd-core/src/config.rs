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

        if !matches!(
            self.providers.default_adapter.as_str(),
            "null" | "local" | "openai_compat" | "ollama" | "opencode"
        ) {
            return Err(ConfigError::UnknownAdapter {
                adapter: self.providers.default_adapter.clone(),
            });
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
    /// Which `VectorIndex` implementation recall uses: "brute-force" (default, oracle)
    /// or "hnsw" (ARCHITECTURE-PLAN §21.12).
    pub vector_index_kind: String,
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
            vector_index_kind: "brute-force".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProviderConfig {
    pub default_adapter: String,
    pub paid_spend_cap_usd: f64,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            default_adapter: "local".to_string(),
            paid_spend_cap_usd: 0.0,
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
        cfg.providers.default_adapter = "ollama".to_string();
        assert!(matches!(
            cfg.validate(),
            Err(ConfigError::PaidProviderRequiresBudget { .. })
        ));
        cfg.providers.paid_spend_cap_usd = 1.0;
        assert!(
            cfg.validate().is_ok(),
            "remote adapter with budget validates"
        );
        cfg.providers.default_adapter = "bogus".to_string();
        assert!(matches!(
            cfg.validate(),
            Err(ConfigError::UnknownAdapter { .. })
        ));
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
