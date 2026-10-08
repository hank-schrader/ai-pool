//! `pool-miner`: serves models from the ai-pool catalog on this machine's GPU.

mod commands;
mod connection;
mod hardware;
mod hosted;
mod plan;
mod pool;
mod selftest;
mod setup;
mod ui;

use std::{path::PathBuf, process::ExitCode, time::Duration};

use clap::{Parser, Subcommand};
use pool_protocol::{Catalog, Model};
use pool_runtime::Cache;
use tracing_subscriber::EnvFilter;

use crate::{connection::Miner, pool::Pool};

#[derive(Parser)]
#[command(name = "pool-miner", version, about = "Serve ai-pool models from this machine's GPU")]
pub struct Cli {
    /// Pool server URL.
    #[arg(long, env = "AI_POOL_URL", default_value = "http://127.0.0.1:8080", global = true)]
    pool: String,
    /// Miner token issued by the pool host (not needed for a local pool).
    #[arg(long, env = "AI_POOL_MINER_TOKEN", hide_env_values = true, global = true)]
    token: Option<String>,
    /// File containing the miner token.
    #[arg(long, global = true)]
    token_file: Option<PathBuf>,
    /// Read the catalog from this file instead of the pool (offline commands).
    #[arg(long, global = true)]
    catalog: Option<PathBuf>,
    /// Models to serve, comma-separated (default: choose interactively).
    #[arg(long, value_delimiter = ',', global = true)]
    models: Vec<String>,
    /// Allow downloads without asking.
    #[arg(long, global = true)]
    yes: bool,
    /// Force a profile: MODEL=PROFILE_ID (repeatable).
    #[arg(long = "profile", value_name = "MODEL=PROFILE", global = true)]
    profiles: Vec<String>,
    /// Force a context size: MODEL=TOKENS, must be a catalog profile (repeatable).
    #[arg(long = "context", value_name = "MODEL=TOKENS", global = true)]
    contexts: Vec<String>,
    /// GPU index to serve from (default: the first).
    #[arg(long, global = true)]
    device: Option<u32>,
    /// Cache directory for runtimes and weights (default: OS cache dir + ai-pool).
    #[arg(long, env = "AI_POOL_CACHE_DIR", global = true)]
    cache_dir: Option<PathBuf>,
    /// Use this llama-server instead of the managed runtime (must report the pinned commit).
    #[arg(long, global = true)]
    llama_server: Option<PathBuf>,
    /// Path to the clef-token-count helper.
    #[arg(long, env = "AI_POOL_CLEF_HELPER", global = true)]
    clef_helper: Option<PathBuf>,
    /// Run a runtime build that has not been qualified on real hardware yet.
    #[arg(long, global = true)]
    allow_unqualified_runtime: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Connect to the pool and serve the selected models (default).
    Run,
    /// Show the pool's models and which profiles fit this machine.
    ListModels,
    /// Download (or import) model weights into the cache.
    Download {
        #[arg(required = true)]
        models: Vec<String>,
        /// Import an existing local weights file instead of downloading (one model).
        #[arg(long)]
        import: Option<PathBuf>,
    },
    /// Check GPU, runtime, helper and cache.
    Doctor,
    /// Manage cached weights.
    Cache {
        #[command(subcommand)]
        action: CacheAction,
    },
    /// Start the selected models locally and exercise them, without a pool.
    #[command(hide = true)]
    Selftest,
}

#[derive(Subcommand)]
enum CacheAction {
    /// List cached models.
    List,
    /// Re-hash a cached model.
    Verify { model: String },
    /// Delete a cached model (refused while a miner serves it).
    Remove { model: String },
}

#[tokio::main]
async fn main() -> ExitCode {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,llama_server=warn"));
    tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr).init();

    let cli = Cli::parse();
    let result = match &cli.command {
        None | Some(Command::Run) => run(&cli).await,
        Some(Command::ListModels) => commands::list_models(&cli).await,
        Some(Command::Download { models, import }) => commands::download(&cli, models, import.as_deref()).await,
        Some(Command::Doctor) => commands::doctor(&cli).await,
        Some(Command::Cache { action }) => match action {
            CacheAction::List => commands::cache_list(&cli).await,
            CacheAction::Verify { model } => commands::cache_verify(&cli, model).await,
            CacheAction::Remove { model } => commands::cache_remove(&cli, model).await,
        },
        Some(Command::Selftest) => selftest::run(&cli).await,
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

impl Cli {
    fn pool(&self) -> Result<Pool, String> {
        Pool::new(&self.pool, Pool::token(self.token.clone(), self.token_file.as_deref())?)
    }

    fn cache(&self) -> Result<Cache, String> {
        Cache::new(self.cache_dir.clone()).map_err(|error| error.to_string())
    }

    /// The catalog from `--catalog` or the pool, with its revision.
    async fn load_catalog(&self, retry: bool) -> Result<(Catalog, String), String> {
        if let Some(path) = &self.catalog {
            let catalog = Catalog::load(path).map_err(|error| error.to_string())?;
            let revision = catalog.revision();
            return Ok((catalog, revision));
        }
        let pool = self.pool()?;
        if retry { pool.catalog_with_retry().await } else { pool.catalog().await }
    }

    fn options(&self) -> Result<setup::Options, String> {
        Ok(setup::Options {
            cache: self.cache()?,
            yes: self.yes,
            overrides: plan::parse_overrides(&self.profiles, &self.contexts)?,
            llama_server: self.llama_server.clone(),
            clef_helper: self.clef_helper.clone(),
            allow_unqualified_runtime: self.allow_unqualified_runtime,
        })
    }

    /// `--models`, or an interactive choice on a terminal.
    fn select<'a>(&self, catalog: &'a Catalog, machine: &hardware::Machine) -> Result<Vec<&'a Model>, String> {
        if !self.models.is_empty() {
            return self
                .models
                .iter()
                .map(|id| {
                    catalog.model(id).ok_or_else(|| {
                        let known: Vec<_> = catalog.models.iter().map(|model| model.id.as_str()).collect();
                        format!("unknown model {id:?} (the pool offers: {})", known.join(", "))
                    })
                })
                .collect();
        }
        if !ui::interactive() {
            return Err("no terminal to choose models; pass --models (see `pool-miner list-models`)".into());
        }
        commands::choose_models(catalog, machine)
    }
}

async fn run(cli: &Cli) -> Result<(), String> {
    let pool = cli.pool()?;
    let (catalog, revision) = cli.load_catalog(true).await?;
    let machine = hardware::detect(cli.device)?;
    eprintln!("Accelerator: {}", machine.describe());
    let selected = cli.select(&catalog, &machine)?;
    if selected.is_empty() {
        return Err("no models selected".into());
    }
    let options = cli.options()?;
    let miner_id = options.cache.miner_id().map_err(|error| error.to_string())?;
    let running = setup::start(&catalog, &selected, &machine, &options).await?;
    if let Err(error) = running.wait_ready(Duration::from_secs(600)).await {
        running.stop().await;
        return Err(error);
    }

    let miner = Miner::new(
        pool,
        miner_id,
        revision,
        machine.hardware(),
        machine.device_id(),
        running.models.clone(),
        running.changes.clone(),
    );
    let session = tokio::spawn(miner.clone().run());
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            eprintln!("Draining: finishing active jobs (Ctrl-C again to stop now)...");
            tokio::select! {
                _ = miner.drain(Duration::from_secs(30)) => {}
                _ = tokio::signal::ctrl_c() => miner.shutdown.cancel(),
            }
        }
        _ = running.shutdown.cancelled() => {}
    }
    miner.shutdown.cancel();
    let _ = session.await;
    running.stop().await;
    eprintln!("Stopped.");
    Ok(())
}
