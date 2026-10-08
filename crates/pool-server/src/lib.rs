//! ai-pool server: accepts client inference requests and routes them to miners.

pub mod api;
pub mod auth;
pub mod config;
pub mod error;
pub mod miners;
pub mod scheduler;

use std::sync::{Arc, atomic::AtomicUsize};

use axum::{
    Router,
    extract::DefaultBodyLimit,
    routing::{get, post},
};
use pool_protocol::Catalog;
use tower_http::trace::TraceLayer;

use crate::{auth::Auth, config::Config, scheduler::SchedulerHandle};

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub catalog: Arc<Catalog>,
    pub catalog_revision: Arc<str>,
    pub auth: Arc<Auth>,
    pub scheduler: SchedulerHandle,
    /// Open miner connections, bounded by `Config::max_miners`.
    pub miner_connections: Arc<AtomicUsize>,
}

impl AppState {
    /// Starts the scheduler task; call inside a Tokio runtime.
    pub fn new(config: Config, catalog: Catalog) -> Self {
        let catalog = Arc::new(catalog);
        Self {
            catalog_revision: catalog.revision().into(),
            auth: Arc::new(Auth::new(&config)),
            scheduler: scheduler::spawn(config.clone(), catalog.clone()),
            miner_connections: Arc::new(AtomicUsize::new(0)),
            catalog,
            config: Arc::new(config),
        }
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(api::healthz))
        .route("/readyz", get(api::readyz))
        .route("/v1/models", get(api::list_models))
        .route("/v1/models/{id}", get(api::get_model))
        .route("/v1/systemone", post(api::systemone))
        .route("/v1/chat/completions", post(api::chat_completions))
        .route("/admin/v1/status", get(api::admin_status))
        .route("/miner/v1/catalog", get(api::miner_catalog))
        .route("/miner/v1/connect", get(miners::connect))
        // per-model limits are checked after parsing; this bounds the read itself
        .layer(DefaultBodyLimit::max(16 << 20))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
