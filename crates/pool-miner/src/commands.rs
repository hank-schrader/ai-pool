//! Informational and cache subcommands.

use std::path::Path;

use pool_protocol::{Catalog, Model};
use pool_runtime::{download, helper, install, manifest};

use crate::{
    Cli,
    hardware::{self, Machine},
    setup,
    ui::{self, Reporter},
};

fn fits(model: &Model, machine: &Machine) -> String {
    let profiles = model.profiles_for(machine.accelerator);
    if profiles.is_empty() {
        return format!("no {} profile", machine.accelerator);
    }
    match profiles.iter().rev().find(|profile| profile.memory_mib <= machine.budget_mib) {
        Some(best) => format!("fits up to {} ({} MiB)", best.id, best.memory_mib),
        None => format!("does not fit (needs {} MiB)", profiles[0].memory_mib),
    }
}

pub fn choose_models<'a>(catalog: &'a Catalog, machine: &Machine) -> Result<Vec<&'a Model>, String> {
    let items: Vec<String> = catalog
        .models
        .iter()
        .map(|model| {
            format!(
                "{} — {} [{}, {}]",
                model.id,
                model.description,
                ui::gib(model.weights.size_bytes),
                fits(model, machine)
            )
        })
        .collect();
    let defaults: Vec<bool> = catalog.models.iter().map(|model| catalog.default_models.contains(&model.id)).collect();
    let picked = dialoguer::MultiSelect::new()
        .with_prompt("Models to serve (space toggles, enter confirms)")
        .items(&items)
        .defaults(&defaults)
        .interact()
        .map_err(|error| error.to_string())?;
    Ok(picked.into_iter().map(|index| &catalog.models[index]).collect())
}

pub async fn list_models(cli: &Cli) -> Result<(), String> {
    let (catalog, _) = cli.load_catalog(false).await?;
    let cache = cli.cache()?;
    let machine = hardware::detect(cli.device);
    match &machine {
        Ok(machine) => println!("Accelerator: {}\n", machine.describe()),
        Err(error) => println!("Accelerator: not detected ({error})\n"),
    }
    for model in &catalog.models {
        let default = if catalog.default_models.contains(&model.id) { " (default)" } else { "" };
        let cached = if setup::looks_cached(&cache, model) { "cached" } else { "not downloaded" };
        println!("{}{default}", model.id);
        println!("  {}", model.description);
        println!("  {:?}, weights {} ({cached})", model.capability(), ui::gib(model.weights.size_bytes));
        for profile in &model.profiles {
            let mark = match &machine {
                Ok(machine) if machine.accelerator == profile.accelerator => {
                    if profile.memory_mib <= machine.budget_mib { "fits" } else { "too large" }
                }
                Ok(_) => "other accelerator",
                Err(_) => "",
            };
            println!(
                "    {:<12} context {:>6}  {:>6} MiB ({:?})  {mark}",
                profile.id, profile.context_tokens, profile.memory_mib, profile.memory_source
            );
        }
        println!();
    }
    Ok(())
}

pub async fn download(cli: &Cli, models: &[String], import: Option<&Path>) -> Result<(), String> {
    let (catalog, _) = cli.load_catalog(false).await?;
    let cache = cli.cache()?;
    if import.is_some() && models.len() != 1 {
        return Err("--import needs exactly one model".into());
    }
    for id in models {
        let model = catalog.model(id).ok_or_else(|| format!("unknown model {id:?}"))?;
        let dest = cache.model_path(&model.weights);
        match import {
            Some(source) => {
                let reporter = Reporter::new(&model.weights.filename);
                download::import_verified(
                    source,
                    &dest,
                    model.weights.size_bytes,
                    &model.weights.sha256,
                    &|stage, done, total| reporter.update(stage, done, total),
                )
                .await
                .map_err(|error| format!("{id}: {error}"))?;
                println!("{id}: imported {} -> {}", source.display(), dest.display());
            }
            None => {
                setup::ensure_weights(&cache, model).await?;
                println!("{id}: {}", dest.display());
            }
        }
    }
    Ok(())
}

pub async fn doctor(cli: &Cli) -> Result<(), String> {
    let mut problems = 0;
    let machine = match hardware::detect(cli.device) {
        Ok(machine) => {
            println!("[ok]   accelerator: {}", machine.describe());
            Some(machine)
        }
        Err(error) => {
            problems += 1;
            println!("[fail] accelerator: {error}");
            None
        }
    };
    let cache = cli.cache()?;
    println!("[ok]   cache: {}", cache.root().display());

    match cli.load_catalog(false).await {
        Ok((catalog, revision)) => {
            println!("[ok]   catalog: {} models, revision {revision}", catalog.models.len());
            if let (Some(machine), Some(model)) = (&machine, catalog.models.first()) {
                match setup::runtime_target(&catalog, model, machine, true) {
                    Ok((spec, target)) => {
                        let qualified = if target.qualified { "qualified" } else { "NOT qualified" };
                        let server = cli.llama_server.clone().or_else(|| {
                            install::is_installed(&cache, spec, target)
                                .then(|| cache.runtime_dir(&spec.id, &target.target).join(&target.server_binary))
                        });
                        match server {
                            Some(server) => match install::check_version(&server, &spec.llama_cpp_commit).await {
                                Ok(version) => {
                                    println!("[ok]   runtime {} {} ({qualified}): {version}", spec.id, target.target)
                                }
                                Err(error) => {
                                    problems += 1;
                                    println!("[fail] runtime: {error}");
                                }
                            },
                            None => println!(
                                "[--]   runtime {} {} ({qualified}): not installed yet",
                                spec.id, target.target
                            ),
                        }
                    }
                    Err(error) => {
                        problems += 1;
                        println!("[fail] runtime: {error}");
                    }
                }
            }
            for model in &catalog.models {
                let state = if setup::looks_cached(&cache, model) { "cached" } else { "not downloaded" };
                println!("[--]   {}: {state}", model.id);
            }
        }
        Err(error) => {
            problems += 1;
            println!("[fail] catalog: {error}");
        }
    }

    match helper::locate(cli.clef_helper.as_deref()) {
        Ok(path) => {
            let output = std::process::Command::new(&path).arg("--version").output();
            match output {
                Ok(output) if output.status.success() => {
                    println!(
                        "[ok]   clef helper: {} ({})",
                        path.display(),
                        String::from_utf8_lossy(&output.stdout).trim()
                    )
                }
                _ => {
                    problems += 1;
                    println!("[fail] clef helper: {} does not run", path.display());
                }
            }
        }
        Err(error) => println!("[--]   clef helper: {error}"),
    }
    println!(
        "[--]   runtime manifest: {} for this build ({})",
        manifest::embedded().runtimes.len(),
        manifest::host_target()
    );
    if problems > 0 { Err(format!("{problems} problem(s) found")) } else { Ok(()) }
}

pub async fn cache_list(cli: &Cli) -> Result<(), String> {
    let (catalog, _) = cli.load_catalog(false).await?;
    let cache = cli.cache()?;
    println!("{}", cache.root().display());
    for model in &catalog.models {
        let path = cache.model_path(&model.weights);
        let state = match std::fs::metadata(&path) {
            Ok(meta) if meta.len() == model.weights.size_bytes => {
                format!("{} at {}", ui::gib(meta.len()), path.display())
            }
            Ok(meta) => format!("incomplete ({} of {} bytes)", meta.len(), model.weights.size_bytes),
            Err(_) => "not cached".into(),
        };
        println!("  {:<24} {state}", model.id);
    }
    Ok(())
}

pub async fn cache_verify(cli: &Cli, id: &str) -> Result<(), String> {
    let (catalog, _) = cli.load_catalog(false).await?;
    let cache = cli.cache()?;
    let model = catalog.model(id).ok_or_else(|| format!("unknown model {id:?}"))?;
    let reporter = Reporter::new(&model.weights.filename);
    download::verify_file(
        &cache.model_path(&model.weights),
        model.weights.size_bytes,
        &model.weights.sha256,
        &|stage, done, total| reporter.update(stage, done, total),
    )
    .await
    .map_err(|error| error.to_string())?;
    println!("{id}: verified");
    Ok(())
}

pub async fn cache_remove(cli: &Cli, id: &str) -> Result<(), String> {
    let (catalog, _) = cli.load_catalog(false).await?;
    let cache = cli.cache()?;
    let model = catalog.model(id).ok_or_else(|| format!("unknown model {id:?}"))?;
    if cache.remove_model(&model.weights).map_err(|error| error.to_string())? {
        println!("{id}: removed");
    } else {
        println!("{id}: not cached");
    }
    Ok(())
}
