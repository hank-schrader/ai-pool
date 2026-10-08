//! Turns a model selection into running runtimes: profile plan, runtime
//! install, verified weights, helper, then one supervised child per model.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use pool_protocol::{Backend, Catalog, Model};
use pool_runtime::{
    Cache, ClefCounter, InstalledRuntime, LaunchSpec, download, helper,
    install::{self, ensure_runtime},
    manifest::{self, RuntimeSpec, TargetSpec},
};
use tokio::{sync::watch, task::JoinHandle};
use tokio_util::sync::CancellationToken;

use crate::{
    hardware::Machine,
    hosted::Hosted,
    plan::{self, Choice, Override},
    ui::{self, Reporter},
};

pub struct Options {
    pub cache: Cache,
    pub yes: bool,
    pub overrides: HashMap<String, Override>,
    pub llama_server: Option<PathBuf>,
    pub clef_helper: Option<PathBuf>,
    pub allow_unqualified_runtime: bool,
}

pub struct Running {
    pub models: Vec<Arc<Hosted>>,
    pub changes: watch::Receiver<u64>,
    pub shutdown: CancellationToken,
    supervisors: Vec<JoinHandle<()>>,
}

impl Running {
    /// Stops every child and waits until they are reaped.
    pub async fn stop(self) {
        self.shutdown.cancel();
        for supervisor in self.supervisors {
            let _ = supervisor.await;
        }
    }

    /// Waits until every model is ready, or fails when one stays down.
    pub async fn wait_ready(&self, timeout: Duration) -> Result<(), String> {
        let deadline = tokio::time::Instant::now() + timeout;
        for (model, supervisor) in self.models.iter().zip(&self.supervisors) {
            let mut ready = model.subscribe();
            while ready.borrow_and_update().is_none() {
                if supervisor.is_finished() {
                    return Err(format!("{} failed to start; see the log above", model.model.id));
                }
                if tokio::time::Instant::now() > deadline {
                    return Err(format!("{} was not ready within {}s", model.model.id, timeout.as_secs()));
                }
                let _ = tokio::time::timeout(Duration::from_millis(500), ready.changed()).await;
            }
        }
        Ok(())
    }
}

pub fn http() -> reqwest::Client {
    reqwest::Client::builder().connect_timeout(Duration::from_secs(20)).build().expect("HTTP client builds")
}

/// The approved runtime target for this machine, refusing unqualified ones
/// unless explicitly allowed.
pub fn runtime_target(
    catalog: &Catalog,
    model: &Model,
    machine: &Machine,
    allow_unqualified: bool,
) -> Result<(&'static RuntimeSpec, &'static TargetSpec), String> {
    let runtime = catalog.runtime(&model.runtime).ok_or_else(|| format!("{}: unknown runtime", model.id))?;
    let spec = manifest::embedded().runtime(&runtime.id).ok_or_else(|| {
        format!("{}: runtime {:?} is not approved by this miner version; update pool-miner", model.id, runtime.id)
    })?;
    if spec.llama_cpp_commit != runtime.llama_cpp_commit {
        return Err(format!("runtime {:?}: the pool and this miner pin different llama.cpp commits", runtime.id));
    }
    let target = spec.target(manifest::host_target(), machine.accelerator).ok_or_else(|| {
        format!("runtime {:?} has no {} build for {}", runtime.id, machine.accelerator, manifest::host_target())
    })?;
    if !target.qualified {
        if !allow_unqualified {
            return Err(format!(
                "the {} {} runtime has not been qualified yet; pass --allow-unqualified-runtime to try it",
                target.target, target.accelerator
            ));
        }
        tracing::warn!("using the unqualified {} {} runtime", target.target, target.accelerator);
    }
    Ok((spec, target))
}

pub fn print_plan(choices: &[Choice]) {
    let total: u64 = choices.iter().map(|choice| choice.profile.memory_mib).sum();
    eprintln!("Profile plan:");
    for choice in choices {
        let source = match choice.profile.memory_source {
            pool_protocol::catalog::MemorySource::Measured => "measured",
            pool_protocol::catalog::MemorySource::Estimate => "estimate",
        };
        eprintln!(
            "  {:<24} {:<12} context {:>6}  {:>6} MiB ({source})",
            choice.model.id, choice.profile.id, choice.profile.context_tokens, choice.profile.memory_mib
        );
    }
    eprintln!("  total {total} MiB");
}

/// Fetches weights unless cached, after consent for large downloads.
pub async fn ensure_weights(cache: &Cache, model: &Model) -> Result<PathBuf, String> {
    let path = cache.model_path(&model.weights);
    let reporter = Reporter::new(&model.weights.filename);
    download::fetch_verified(
        &http(),
        &model.weights.url,
        &path,
        model.weights.size_bytes,
        &model.weights.sha256,
        &|stage, done, total| reporter.update(stage, done, total),
    )
    .await
    .map_err(|error| format!("{}: {error}", model.id))?;
    Ok(path)
}

/// Whether the weights file is present with the right size (not hashed).
pub fn looks_cached(cache: &Cache, model: &Model) -> bool {
    std::fs::metadata(cache.model_path(&model.weights)).is_ok_and(|meta| meta.len() == model.weights.size_bytes)
}

pub fn confirm_downloads(cache: &Cache, models: &[&Model], yes: bool) -> Result<(), String> {
    let missing: Vec<_> = models.iter().filter(|model| !looks_cached(cache, model)).collect();
    if missing.is_empty() || yes {
        return Ok(());
    }
    let total: u64 = missing.iter().map(|model| model.weights.size_bytes).sum();
    let names: Vec<_> =
        missing.iter().map(|model| format!("{} ({})", model.id, ui::gib(model.weights.size_bytes))).collect();
    let question = format!("Download {} ({} total)?", names.join(", "), ui::gib(total));
    if !ui::interactive() {
        return Err(format!("{question} Pass --yes to allow the download."));
    }
    let confirmed =
        dialoguer::Confirm::new().with_prompt(question).default(true).interact().map_err(|e| e.to_string())?;
    if confirmed { Ok(()) } else { Err("download declined".into()) }
}

pub async fn install_runtime(
    catalog: &Catalog,
    model: &Model,
    machine: &Machine,
    options: &Options,
) -> Result<InstalledRuntime, String> {
    let (spec, target) = runtime_target(catalog, model, machine, options.allow_unqualified_runtime)?;
    if options.llama_server.is_none() && !install::is_installed(&options.cache, spec, target) {
        let size: u64 = target.archives.iter().map(|archive| archive.size_bytes).sum();
        eprintln!("Installing runtime {} for {} ({})", spec.id, target.target, ui::gib(size));
    }
    let reporter = Reporter::new(format!("runtime {}", spec.release_tag));
    ensure_runtime(&http(), &options.cache, spec, target, options.llama_server.as_deref(), &|stage, done, total| {
        reporter.update(stage, done, total)
    })
    .await
    .map_err(|error| error.to_string())
}

/// Plans, installs and starts the selected models.
pub async fn start(
    catalog: &Catalog,
    selected: &[&Model],
    machine: &Machine,
    options: &Options,
) -> Result<Running, String> {
    let choices = plan::plan(selected, machine.accelerator, machine.budget_mib, &options.overrides)?;
    print_plan(&choices);
    confirm_downloads(&options.cache, selected, options.yes)?;

    let mut runtimes: HashMap<String, InstalledRuntime> = HashMap::new();
    let mut prepared = Vec::new();
    for choice in &choices {
        let model = choice.model;
        if !runtimes.contains_key(&model.runtime) {
            let runtime = install_runtime(catalog, model, machine, options).await?;
            tracing::info!("runtime {}: {}", model.runtime, runtime.version);
            runtimes.insert(model.runtime.clone(), runtime);
        }
        let weights = ensure_weights(&options.cache, model).await?;
        prepared.push((choice, weights));
    }

    let (changes_tx, changes) = watch::channel(0u64);
    let shutdown = CancellationToken::new();
    let mut models = Vec::new();
    let mut supervisors = Vec::new();
    for (choice, weights) in prepared {
        let model = choice.model;
        let runtime = &runtimes[&model.runtime];
        let commit = &catalog.runtime(&model.runtime).expect("validated").llama_cpp_commit;
        let counter = match model.backend {
            Backend::LlamaServerSystemone => {
                Some(start_counter(options.clef_helper.as_deref(), &weights, commit).await?)
            }
            Backend::LlamaServerChat => None,
        };
        let launch = LaunchSpec {
            label: model.id.clone(),
            server: runtime.server.clone(),
            weights,
            backend: model.backend,
            context_tokens: choice.profile.context_tokens,
            gpu_layers: model.execution.gpu_layers,
            batch_tokens: model.execution.batch_tokens,
            microbatch_tokens: model.execution.microbatch_tokens,
            cuda_device: machine.cuda_device(),
        };
        let in_use = options.cache.hold_model(&model.weights).map_err(|error| error.to_string())?;
        // a forced profile is kept as is; a planned one may step down if its probe fails
        let profiles: Vec<_> = if options.overrides.contains_key(&model.id) {
            vec![choice.profile.clone()]
        } else {
            let mut smaller: Vec<_> = model
                .profiles_for(machine.accelerator)
                .into_iter()
                .filter(|profile| profile.context_tokens <= choice.profile.context_tokens)
                .cloned()
                .collect();
            smaller.sort_by_key(|profile| std::cmp::Reverse(profile.context_tokens));
            smaller
        };
        let hosted = Arc::new(Hosted::new(
            model.clone(),
            catalog.model_revision(model),
            profiles,
            machine.device_id(),
            launch,
            counter,
            in_use,
        ));
        supervisors.push(hosted.supervise(changes_tx.clone(), shutdown.clone()));
        models.push(hosted);
    }
    Ok(Running { models, changes, shutdown, supervisors })
}

async fn start_counter(explicit: Option<&Path>, weights: &Path, commit: &str) -> Result<ClefCounter, String> {
    let path = helper::locate(explicit).map_err(|error| error.to_string())?;
    let counter = ClefCounter::start(path, weights.to_path_buf(), commit).await.map_err(|error| error.to_string())?;
    tracing::info!("Clef token counter: {}", counter.path().display());
    Ok(counter)
}
