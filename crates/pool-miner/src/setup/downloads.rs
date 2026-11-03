use super::*;

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
