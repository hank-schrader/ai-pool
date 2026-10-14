//! ai-pool server: accepts client inference requests and routes them to miners.

pub mod api;
pub mod assets;
pub mod auth;
pub mod config;
pub mod error;
pub mod miners;
pub mod scheduler;
pub mod stats;
pub mod stats_stream;

use std::sync::{Arc, atomic::AtomicUsize};

use axum::{
    Router,
    routing::{get, post},
};
use pool_protocol::{Backend, Catalog};
use tower_http::trace::TraceLayer;

use crate::{auth::Auth, config::Config, scheduler::SchedulerHandle};

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub catalog: Arc<Catalog>,
    pub catalog_revision: Arc<str>,
    /// The catalog without h2o models, for miners 0.1.3 that know offload but not h2o.
    pub offload_catalog: Arc<Catalog>,
    pub offload_catalog_revision: Arc<str>,
    /// The catalog without offload profiles and h2o models, for miners up to 0.1.2.
    pub legacy_catalog: Arc<Catalog>,
    pub legacy_catalog_revision: Arc<str>,
    pub auth: Arc<Auth>,
    pub scheduler: SchedulerHandle,
    pub stats_publisher: stats_stream::StatsPublisher,
    /// Open miner connections, bounded by `Config::max_miners`.
    pub miner_connections: Arc<AtomicUsize>,
}

impl AppState {
    /// Starts the scheduler task; call inside a Tokio runtime.
    pub fn new(config: Config, catalog: Catalog) -> Self {
        let mut offload = catalog.clone();
        offload.models.retain(|model| model.backend != Backend::LlamaServerH2o);
        let mut legacy = offload.clone();
        for model in &mut legacy.models {
            model.profiles.retain(|profile| !profile.offloaded());
        }
        let catalog = Arc::new(catalog);
        let scheduler = scheduler::spawn(config.clone(), catalog.clone());
        let stats_publisher = stats_stream::StatsPublisher::spawn(scheduler.stats_receiver(), config.max_stats_streams);
        Self {
            catalog_revision: catalog.revision().into(),
            offload_catalog_revision: offload.revision().into(),
            offload_catalog: Arc::new(offload),
            legacy_catalog_revision: legacy.revision().into(),
            legacy_catalog: Arc::new(legacy),
            auth: Arc::new(Auth::new(&config)),
            scheduler,
            stats_publisher,
            miner_connections: Arc::new(AtomicUsize::new(0)),
            catalog,
            config: Arc::new(config),
        }
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(assets::index))
        .route("/assets/{*path}", get(assets::asset))
        .route("/v1/stats", get(api::stats))
        .route("/v1/stats/stream", get(api::stats_stream))
        .route("/healthz", get(api::healthz))
        .route("/readyz", get(api::readyz))
        .route("/v1/models", get(api::list_models))
        .route("/v1/models/{id}", get(api::get_model))
        .route("/v1/systemone", post(api::systemone))
        .route("/v1/chat/completions", post(api::chat_completions))
        .route("/admin/v1/status", get(api::admin_status))
        .route("/miner/v1/catalog", get(api::miner_catalog))
        .route("/miner/v1/connect", get(miners::connect))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
