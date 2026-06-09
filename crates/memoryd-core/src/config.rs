use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;

pub const DEFAULT_BIND: &str = "127.0.0.1:7077";

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

        if self.providers.default_adapter != "null" && self.providers.paid_spend_cap_usd == 0.0 {
            return Err(ConfigError::PaidProviderRequiresBudget {
                adapter: self.providers.default_adapter.clone(),
            });
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
            default_adapter: "null".to_string(),
            paid_spend_cap_usd: 0.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ConfigError {
    RemoteBindRequiresBearer { bind: SocketAddr },
    PaidProviderRequiresBudget { adapter: String },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RemoteBindRequiresBearer { bind } => {
                write!(f, "non-loopback bind {bind} requires a bearer token")
            }
            Self::PaidProviderRequiresBudget { adapter } => write!(
                f,
                "provider adapter {adapter} is not allowed with a zero paid-spend budget"
            ),
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
    fn default_config_is_localhost_null_provider_zero_spend() {
        let cfg = Config::with_db_path(PathBuf::from("memoryd.db"));

        assert_eq!(cfg.bind.to_string(), DEFAULT_BIND);
        assert_eq!(cfg.providers.default_adapter, "null");
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

        cfg.bearer_token = Some("test-token".to_string());
        assert!(cfg.validate().is_ok());
    }
}
