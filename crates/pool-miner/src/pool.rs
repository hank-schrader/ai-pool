//! The pool's miner endpoints.

use std::{path::Path, time::Duration};

use pool_protocol::Catalog;
use serde::Deserialize;

#[derive(Clone, Debug)]
pub struct Pool {
    base: String,
    token: Option<String>,
}

#[derive(Deserialize)]
struct CatalogReply {
    catalog_revision: String,
    catalog: serde_json::Value,
}

impl Pool {
    pub fn new(url: &str, token: Option<String>) -> Result<Self, String> {
        let base = url.trim_end_matches('/').to_string();
        if !(base.starts_with("http://") || base.starts_with("https://")) {
            return Err(format!("--pool {url:?}: expected an http:// or https:// URL"));
        }
        Ok(Self { base, token })
    }

    /// Reads `--token` or `--token-file`.
    pub fn token(token: Option<String>, token_file: Option<&Path>) -> Result<Option<String>, String> {
        match (token, token_file) {
            (Some(_), Some(_)) => Err("use either --token or --token-file, not both".into()),
            (Some(token), None) => Ok(Some(token)),
            (None, Some(path)) => std::fs::read_to_string(path)
                .map(|text| Some(text.trim().to_string()))
                .map_err(|error| format!("cannot read {}: {error}", path.display())),
            (None, None) => Ok(None),
        }
    }

    pub fn url(&self) -> &str {
        &self.base
    }

    pub fn token_value(&self) -> Option<&str> {
        self.token.as_deref()
    }

    pub fn connect_url(&self) -> String {
        let rest = self.base.strip_prefix("https://").map(|rest| format!("wss://{rest}"));
        let url = rest.unwrap_or_else(|| self.base.replacen("http://", "ws://", 1));
        format!("{url}/miner/v1/connect")
    }

    /// Fetches and validates the catalog. The revision must match what this
    /// miner computes, or the two disagree about the catalog's meaning.
    pub async fn catalog(&self) -> Result<(Catalog, String), String> {
        let http = reqwest::Client::builder().timeout(Duration::from_secs(30)).build().map_err(|e| e.to_string())?;
        // offload profiles are only sent to miners that understand them
        let mut request = http.get(format!("{}/miner/v1/catalog?features=offload", self.base));
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        let response =
            request.send().await.map_err(|error| format!("cannot reach the pool at {}: {error}", self.base))?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(format!("the pool refused the catalog request (HTTP {status}): {}", body.trim()));
        }
        let reply: CatalogReply = response.json().await.map_err(|error| format!("invalid catalog reply: {error}"))?;
        let catalog = Catalog::parse(&reply.catalog.to_string()).map_err(|error| error.to_string())?;
        if catalog.revision() != reply.catalog_revision {
            tracing::warn!(
                "catalog revision mismatch (pool {}, computed {}); the pool may run a different version",
                reply.catalog_revision,
                catalog.revision()
            );
        }
        Ok((catalog, reply.catalog_revision))
    }

    /// Retries until the pool answers, for miners started before their pool.
    pub async fn catalog_with_retry(&self) -> Result<(Catalog, String), String> {
        let mut wait = Duration::from_secs(1);
        loop {
            match self.catalog().await {
                Ok(catalog) => return Ok(catalog),
                Err(error) if error.starts_with("cannot reach") => {
                    tracing::warn!("{error}; retrying in {}s", wait.as_secs());
                    tokio::time::sleep(wait).await;
                    wait = (wait * 2).min(Duration::from_secs(30));
                }
                Err(error) => return Err(error),
            }
        }
    }
}
