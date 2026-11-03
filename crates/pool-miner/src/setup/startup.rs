use super::*;

pub(super) async fn start_prepared(
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
            chat_json: model.execution.chat_json,
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
pub(super) fn startup_profiles(
    choice: &Choice<'_>,
    accelerator: pool_protocol::Accelerator,
    ram_budget_mib: u64,
    exact: bool,
) -> Vec<(pool_protocol::Profile, Placement)> {
    if exact || !choice.model.execution.allow_ram_fallback || choice.placement != Placement::Published {
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
