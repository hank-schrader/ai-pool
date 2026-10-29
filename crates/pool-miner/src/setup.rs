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
    plan::{self, Choice, Override, Placement},
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
        let question = format!(
            "The {} {} runtime has not completed qualification. Use it anyway?",
            target.target, target.accelerator
        );
        let allowed = allow_unqualified
            || (ui::interactive()
                && dialoguer::Confirm::new()
                    .with_prompt(question)
                    .default(true)
                    .interact()
                    .map_err(|e| e.to_string())?);
        if !allowed {
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
    let total: u64 = choices.iter().map(Choice::memory_mib).sum();
    let host: u64 = choices.iter().map(Choice::host_memory_mib).sum();
    eprintln!("Profile plan:");
    for choice in choices {
        let source = match (choice.placement, choice.profile.memory_source) {
            (Placement::Published, pool_protocol::catalog::MemorySource::Measured) => "measured",
            _ => "estimate",
        };
        let mut offload = match choice.placement {
            Placement::Ram { host_memory_mib, .. } => {
                format!(" + {host_memory_mib} MiB RAM (estimate), automatic RAM fallback (-ngl 0)")
            }
            Placement::Published => match (choice.profile.gpu_layers, choice.profile.host_memory_mib) {
                (Some(layers), Some(ram)) => format!(" + {ram} MiB RAM, {layers} layers on the GPU"),
                _ => String::new(),
            },
        };
        if choice.profile.slots() > 1 {
            offload.push_str(&format!(", {} cache slots", choice.profile.slots()));
        }
        eprintln!(
            "  {:<24} {:<18} context {:>6}  {:>6} MiB ({source}){offload}",
            choice.model.id,
            choice.profile.id,
            choice.profile.context_tokens,
            choice.memory_mib()
        );
    }
    let estimate = if choices.iter().any(|choice| choice.placement != Placement::Published) {
        " (includes automatic RAM fallback estimates)"
    } else {
        ""
    };
    if host > 0 {
        eprintln!("  total {total} MiB accelerator memory + {host} MiB system RAM{estimate}");
    } else {
        eprintln!("  total {total} MiB");
    }
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
    if let Some(minimum) = &target.min_driver {
        pool_runtime::requirements::ensure(&[pool_runtime::requirements::driver(&machine.device().driver, minimum)])
            .map_err(|error| error.to_string())?;
    }
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
    let choices =
        plan::plan(selected, machine.accelerator, machine.budget_mib, machine.ram_budget_mib, &options.overrides)?;
    print_plan(&choices);
    preflight(catalog, selected, machine, options)?;
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

    start_prepared(catalog, &prepared, &runtimes, machine, options, false).await
}

pub struct Prepared {
    runtime: InstalledRuntime,
    weights: PathBuf,
}

pub async fn prepare(
    catalog: &Catalog,
    model: &Model,
    machine: &Machine,
    options: &Options,
) -> Result<Prepared, String> {
    preflight(catalog, &[model], machine, options)?;
    let runtime = install_runtime(catalog, model, machine, options).await?;
    let weights = ensure_weights(&options.cache, model).await?;
    Ok(Prepared { runtime, weights })
}

pub fn disk_check(
    catalog: &Catalog,
    selected: &[&Model],
    accelerator: pool_protocol::Accelerator,
    cache: &Cache,
    override_server: bool,
) -> Result<pool_runtime::requirements::Check, String> {
    use pool_runtime::requirements;
    let mut downloads = 0u64;
    let mut extraction = 0u64;
    let mut runtimes = std::collections::HashSet::new();
    let mut weights = std::collections::HashSet::new();
    for model in selected {
        if weights.insert(&model.weights.sha256) {
            downloads = downloads.saturating_add(requirements::download_bytes(
                &cache.model_path(&model.weights),
                model.weights.size_bytes,
            ));
        }
        if !override_server && runtimes.insert(&model.runtime) {
            let runtime = catalog.runtime(&model.runtime).ok_or("unknown runtime in the catalog")?;
            let spec = manifest::embedded().runtime(&model.runtime).ok_or("runtime is not approved by this miner")?;
            if runtime.llama_cpp_commit != spec.llama_cpp_commit {
                return Err("the pool and this miner pin different llama.cpp commits".into());
            }
            let target = spec.target(manifest::host_target(), accelerator).ok_or("runtime target unavailable")?;
            if !install::is_installed(cache, spec, target) {
                for archive in &target.archives {
                    downloads = downloads.saturating_add(requirements::download_bytes(
                        &cache.download_path(&archive.sha256, &archive.name),
                        archive.size_bytes,
                    ));
                    extraction = extraction.saturating_add(archive.size_bytes);
                }
            }
        }
    }
    let needed = requirements::disk_bytes(downloads, extraction);
    let free = requirements::free_bytes(cache.root()).map_err(|error| error.to_string())?;
    Ok(requirements::disk(cache.root(), needed, free))
}

pub fn preflight(catalog: &Catalog, selected: &[&Model], machine: &Machine, options: &Options) -> Result<(), String> {
    let mut checks =
        vec![disk_check(catalog, selected, machine.accelerator, &options.cache, options.llama_server.is_some())?];
    let mut runtimes = std::collections::HashSet::new();
    for model in selected {
        if runtimes.insert(&model.runtime) {
            let (_, target) = runtime_target(catalog, model, machine, options.allow_unqualified_runtime)?;
            if let Some(minimum) = &target.min_driver {
                checks.push(pool_runtime::requirements::driver(&machine.device().driver, minimum));
            }
        }
    }
    checks.extend(pool_runtime::requirements::platform());
    for check in &checks {
        eprintln!("{}", check.line());
    }
    pool_runtime::requirements::ensure(&checks).map_err(|error| error.to_string())
}

pub async fn start_exact(
    catalog: &Catalog,
    choice: &Choice<'_>,
    prepared: &Prepared,
    machine: &Machine,
    options: &Options,
) -> Result<Running, String> {
    let runtimes = HashMap::from([(choice.model.runtime.clone(), prepared.runtime.clone())]);
    start_prepared(catalog, &[(choice, prepared.weights.clone())], &runtimes, machine, options, true).await
}

async fn start_prepared(
    catalog: &Catalog,
    prepared: &[(&Choice<'_>, PathBuf)],
    runtimes: &HashMap<String, InstalledRuntime>,
    machine: &Machine,
    options: &Options,
    exact: bool,
) -> Result<Running, String> {
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
                Some(start_counter(options.clef_helper.as_deref(), weights, commit).await?)
            }
            Backend::LlamaServerChat | Backend::LlamaServerH2o => None,
        };
        let launch = LaunchSpec {
            label: model.id.clone(),
            server: runtime.server.clone(),
            weights: weights.clone(),
            backend: model.backend,
            accelerator: machine.accelerator,
            context_tokens: choice.profile.context_tokens,
            slots: choice.profile.slots(),
            gpu_layers: model.execution.gpu_layers,
            batch_tokens: model.execution.batch_tokens,
            microbatch_tokens: model.execution.microbatch_tokens,
            cuda_device: machine.cuda_device(),
            cpu_threads: machine.cpu_threads,
        };
        let in_use = options.cache.hold_model(&model.weights).map_err(|error| error.to_string())?;
        let profiles = startup_profiles(
            choice,
            machine.accelerator,
            machine.ram_budget_mib,
            exact || options.overrides.contains_key(&model.id),
        );
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

// Exact benchmark choices and forced profiles never step down after a failed probe.
fn startup_profiles(
    choice: &Choice<'_>,
    accelerator: pool_protocol::Accelerator,
    ram_budget_mib: u64,
    exact: bool,
) -> Vec<(pool_protocol::Profile, Placement)> {
    if exact || choice.placement != Placement::Published {
        return vec![(choice.profile.clone(), choice.placement)];
    }
    let mut fallback: Vec<_> = choice
        .model
        .profiles_for(accelerator)
        .into_iter()
        .filter(|profile| {
            profile.id == choice.profile.id
                || (profile.memory_mib <= choice.profile.memory_mib
                    && profile.preference() < choice.profile.preference()
                    && profile.host_memory_mib.unwrap_or(0) <= ram_budget_mib)
        })
        .cloned()
        .collect();
    fallback.sort_by_key(|profile| std::cmp::Reverse(profile.preference()));
    fallback.sort_by_key(|profile| profile.id != choice.profile.id);
    fallback.into_iter().map(|profile| (profile, Placement::Published)).collect()
}

async fn start_counter(explicit: Option<&Path>, weights: &Path, commit: &str) -> Result<ClefCounter, String> {
    let path = helper::locate(explicit).map_err(|error| error.to_string())?;
    let counter = ClefCounter::start(path, weights.to_path_buf(), commit).await.map_err(|error| error.to_string())?;
    tracing::info!("Clef token counter: {}", counter.path().display());
    Ok(counter)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pool_protocol::Accelerator;

    #[test]
    fn exact_startup_preserves_profile_and_placement_without_step_down() {
        let catalog = Catalog::parse(include_str!("../../../tests/fixtures/all-models.json")).unwrap();
        let model = catalog.model("h2o-q8").unwrap();
        let choice =
            Choice { model, profile: model.profile("cuda-40960-x4").unwrap(), placement: Placement::Published };
        assert!(startup_profiles(&choice, Accelerator::Cuda, 20000, false).len() > 1);
        let exact = startup_profiles(&choice, Accelerator::Cuda, 20000, true);
        assert_eq!(exact.len(), 1);
        assert_eq!(exact[0].0.id, "cuda-40960-x4");
        let ram = Choice { placement: plan::ram_fallback(model, choice.profile).unwrap(), ..choice };
        let exact = startup_profiles(&ram, Accelerator::Cuda, 20000, true);
        assert_eq!(exact.len(), 1);
        assert_eq!(exact[0].1, ram.placement);
    }

    #[test]
    fn smaller_context_can_step_down_with_the_same_measured_memory() {
        let catalog = Catalog::parse(include_str!("../../../config/models.json")).unwrap();
        let model = catalog.model("h2o-q8").unwrap();
        let choice = Choice { model, profile: model.profile("cpu-8192").unwrap(), placement: Placement::Published };
        let profiles = startup_profiles(&choice, Accelerator::Cpu, 0, false);
        assert_eq!(profiles.iter().map(|(p, _)| p.id.as_str()).collect::<Vec<_>>(), ["cpu-8192", "cpu-4096"]);
    }

    #[test]
    fn disk_plan_reserves_each_weight_once_and_honors_runtime_overrides() {
        let catalog = Catalog::parse(include_str!("../../../tests/fixtures/all-models.json")).unwrap();
        let mut model = catalog.model("h2o-q8").unwrap().clone();
        model.weights.size_bytes = 1_000_000_000;
        let root = std::env::temp_dir().join(format!("ai-pool-disk-plan-{}", uuid::Uuid::new_v4()));
        let cache = Cache::new(Some(root.clone())).unwrap();
        let check = disk_check(&catalog, &[&model, &model], Accelerator::Cuda, &cache, true).unwrap();
        assert!(check.message.contains("1.54 GB needed"), "{}", check.message);
        let mut other = model.clone();
        other.weights.sha256 = "f".repeat(64);
        let check = disk_check(&catalog, &[&model, &other], Accelerator::Cuda, &cache, true).unwrap();
        assert!(check.message.contains("2.54 GB needed"), "{}", check.message);
        let spec = manifest::embedded().runtime(&model.runtime).unwrap();
        if let Some(target) = spec.target(manifest::host_target(), Accelerator::Cuda) {
            let check = disk_check(&catalog, &[&model, &other], Accelerator::Cuda, &cache, false).unwrap();
            let archives = target.archives.iter().map(|archive| archive.size_bytes).sum::<u64>();
            let needed = pool_runtime::requirements::disk_bytes(2_000_000_000 + archives, archives);
            assert!(check.message.contains(&format!("{:.2} GB needed", needed as f64 / 1e9)), "{}", check.message);
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
