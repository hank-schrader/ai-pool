//! Bearer credentials. Keys are compared by SHA-256 digest so lookups do not
//! depend on how many leading characters of a guess are right.

use std::collections::HashMap;

use axum::http::{HeaderMap, header};
use sha2::{Digest, Sha256};

use crate::{
    config::{AuthMode, Config, MinerAuth},
    error::ApiError,
};

pub struct Auth {
    mode: AuthMode,
    miner_auth: MinerAuth,
    clients: HashMap<[u8; 32], String>,
    miners: HashMap<[u8; 32], String>,
    admin: Option<[u8; 32]>,
}

impl Auth {
    pub fn new(config: &Config) -> Self {
        let labeled = |keys: &[String], prefix: &str| {
            keys.iter().enumerate().map(|(index, key)| (digest(key), format!("{prefix}-{}", index + 1))).collect()
        };
        Self {
            mode: config.auth_mode,
            miner_auth: config.miner_auth,
            clients: labeled(&config.client_api_keys, "client"),
            miners: labeled(&config.miner_tokens, "miner"),
            admin: config.admin_token.as_deref().map(digest),
        }
    }

    pub fn mode(&self) -> AuthMode {
        self.mode
    }

    /// Returns the client label used for fairness and logs.
    pub fn client(&self, headers: &HeaderMap) -> Result<String, ApiError> {
        match self.mode {
            AuthMode::Local => Ok("local".into()),
            AuthMode::Keys => bearer(headers)
                .and_then(|key| self.clients.get(&digest(key)).cloned())
                .ok_or_else(ApiError::unauthorized),
        }
    }

    pub fn miner(&self, headers: &HeaderMap) -> Result<String, ApiError> {
        let known = bearer(headers).and_then(|key| self.miners.get(&digest(key)).cloned());
        match (self.mode, self.miner_auth) {
            (AuthMode::Local, _) => Ok("local-miner".into()),
            (AuthMode::Keys, MinerAuth::Token) => known.ok_or_else(ApiError::unauthorized),
            // open pools take any miner; a valid token still gives it a name
            (AuthMode::Keys, MinerAuth::Open) => Ok(known.unwrap_or_else(|| "anonymous".into())),
        }
    }

    pub fn admin(&self, headers: &HeaderMap) -> Result<(), ApiError> {
        match (self.mode, self.admin) {
            (AuthMode::Local, _) => Ok(()),
            (AuthMode::Keys, Some(admin)) if bearer(headers).is_some_and(|key| digest(key) == admin) => Ok(()),
            _ => Err(ApiError::unauthorized()),
        }
    }
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers.get(header::AUTHORIZATION)?.to_str().ok()?.strip_prefix("Bearer ").map(str::trim)
}

fn digest(key: &str) -> [u8; 32] {
    Sha256::digest(key.as_bytes()).into()
}
