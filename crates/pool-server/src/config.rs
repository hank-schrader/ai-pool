//! Operational settings from `POOL_*` environment variables (optionally a `.env`
//! file). Every setting has a default that works for a local pool; see
//! `.env.example`.

use std::{env, net::SocketAddr, path::PathBuf, str::FromStr, time::Duration};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthMode {
    /// No credentials; only allowed on a loopback bind address.
    Local,
    /// Client API keys, miner tokens and an optional admin token are required.
    Keys,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub bind: SocketAddr,
    pub catalog: PathBuf,
    pub auth_mode: AuthMode,
    pub client_api_keys: Vec<String>,
    pub miner_tokens: Vec<String>,
    pub admin_token: Option<String>,
    pub request_timeout: Duration,
    pub count_timeout: Duration,
    pub queue_timeout: Duration,
    pub dispatch_ack_timeout: Duration,
    pub first_chunk_timeout: Duration,
    pub queue_capacity_global: usize,
    pub queue_capacity_per_model: usize,
    pub max_active_per_client: usize,
    pub max_queued_per_client: usize,
    pub heartbeat: Duration,
    pub stream_buffer_events: usize,
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        let auth_mode = match var("POOL_AUTH_MODE").as_deref().unwrap_or("local") {
            "local" => AuthMode::Local,
            "keys" => AuthMode::Keys,
            other => return Err(format!("POOL_AUTH_MODE must be local or keys, not {other:?}")),
        };
        let config = Self {
            bind: parse("POOL_BIND", "127.0.0.1:8080")?,
            catalog: PathBuf::from(var("POOL_CATALOG").unwrap_or_else(|| "config/models.json".into())),
            auth_mode,
            client_api_keys: list("POOL_CLIENT_API_KEYS"),
            miner_tokens: list("POOL_MINER_TOKENS"),
            admin_token: var("POOL_ADMIN_TOKEN"),
            request_timeout: seconds("POOL_REQUEST_TIMEOUT_SECONDS", 180)?,
            count_timeout: seconds("POOL_COUNT_TIMEOUT_SECONDS", 10)?,
            queue_timeout: seconds("POOL_QUEUE_TIMEOUT_SECONDS", 15)?,
            dispatch_ack_timeout: seconds("POOL_DISPATCH_ACK_TIMEOUT_SECONDS", 5)?,
            first_chunk_timeout: seconds("POOL_FIRST_CHUNK_TIMEOUT_SECONDS", 60)?,
            queue_capacity_global: parse("POOL_QUEUE_CAPACITY_GLOBAL", "64")?,
            queue_capacity_per_model: parse("POOL_QUEUE_CAPACITY_PER_MODEL", "32")?,
            max_active_per_client: parse("POOL_MAX_ACTIVE_PER_CLIENT", "2")?,
            max_queued_per_client: parse("POOL_MAX_QUEUED_PER_CLIENT", "8")?,
            heartbeat: seconds("POOL_HEARTBEAT_SECONDS", 10)?,
            stream_buffer_events: parse("POOL_STREAM_BUFFER_EVENTS", "512")?,
        };
        config.check()?;
        Ok(config)
    }

    fn check(&self) -> Result<(), String> {
        match self.auth_mode {
            AuthMode::Local if !self.bind.ip().is_loopback() => Err(format!(
                "POOL_AUTH_MODE=local only serves loopback, but POOL_BIND is {}; \
                 set POOL_AUTH_MODE=keys with POOL_CLIENT_API_KEYS and POOL_MINER_TOKENS to serve other machines",
                self.bind
            )),
            AuthMode::Keys if self.client_api_keys.is_empty() || self.miner_tokens.is_empty() => {
                Err("POOL_AUTH_MODE=keys needs POOL_CLIENT_API_KEYS and POOL_MINER_TOKENS".into())
            }
            _ if self.max_active_per_client == 0 || self.queue_capacity_global == 0 => {
                Err("POOL_MAX_ACTIVE_PER_CLIENT and POOL_QUEUE_CAPACITY_GLOBAL must be positive".into())
            }
            _ if self.queue_timeout >= self.request_timeout => {
                Err("POOL_QUEUE_TIMEOUT_SECONDS must be shorter than POOL_REQUEST_TIMEOUT_SECONDS".into())
            }
            _ => Ok(()),
        }
    }
}

fn var(name: &str) -> Option<String> {
    env::var(name).ok().map(|value| value.trim().to_owned()).filter(|value| !value.is_empty())
}

fn list(name: &str) -> Vec<String> {
    var(name)
        .map(|value| value.split(',').map(str::trim).filter(|v| !v.is_empty()).map(str::to_owned).collect())
        .unwrap_or_default()
}

fn parse<T: FromStr>(name: &str, default: &str) -> Result<T, String> {
    let value = var(name).unwrap_or_else(|| default.into());
    value.parse().map_err(|_| format!("{name}={value:?} is not valid"))
}

fn seconds(name: &str, default: u64) -> Result<Duration, String> {
    let value: u64 = parse(name, &default.to_string())?;
    if value == 0 {
        return Err(format!("{name} must be positive"));
    }
    Ok(Duration::from_secs(value))
}
