use std::process::ExitCode;

use clap::{Parser, Subcommand};
use pool_protocol::Catalog;
use pool_server::{AppState, config::Config, router};
use tracing::{error, info};

#[derive(Parser)]
#[command(version, about = "ai-pool server: routes client requests to connected miners")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Serve the API (the default).
    Serve,
    /// Validate the settings and model catalog, then exit.
    CheckConfig,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let dotenv = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,tower_http=info".parse().expect("valid filter")),
        )
        .init();
    if let Ok(path) = dotenv {
        info!(path = %path.display(), "loaded settings file");
    }

    let config = match Config::from_env() {
        Ok(config) => config,
        Err(message) => {
            error!("{message}");
            return ExitCode::FAILURE;
        }
    };
    let catalog = match Catalog::load(&config.catalog) {
        Ok(catalog) => catalog,
        Err(problem) => {
            error!(catalog = %config.catalog.display(), "{problem}");
            return ExitCode::FAILURE;
        }
    };

    if matches!(cli.command, Some(Command::CheckConfig)) {
        println!("settings: ok (bind {}, auth {:?})", config.bind, config.auth_mode);
        println!("catalog {}: ok, revision {}", config.catalog.display(), catalog.revision());
        for model in &catalog.models {
            let profiles: Vec<_> = model.profiles.iter().map(|p| p.id.as_str()).collect();
            println!("  {} ({}): {}", model.id, model.capability(), profiles.join(", "));
        }
        return ExitCode::SUCCESS;
    }

    let state = AppState::new(config, catalog);
    let app = router(state.clone());

    let listener = match tokio::net::TcpListener::bind(state.config.bind).await {
        Ok(listener) => listener,
        Err(problem) => {
            error!(bind = %state.config.bind, "cannot listen: {problem}");
            return ExitCode::FAILURE;
        }
    };
    info!(
        bind = %state.config.bind,
        auth = ?state.config.auth_mode,
        catalog_revision = %state.catalog_revision,
        models = ?state.catalog.models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
        "pool server listening"
    );
    let shutdown = async {
        let _ = tokio::signal::ctrl_c().await;
        info!("shutting down");
    };
    if let Err(problem) = axum::serve(listener, app).with_graceful_shutdown(shutdown).await {
        error!("server error: {problem}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
