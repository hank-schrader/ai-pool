//! Chooses one published profile per selected model so that all of them fit
//! the device together.

use std::collections::HashMap;

use pool_protocol::{Accelerator, Model, Profile};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Override {
    Profile(String),
    Context(u32),
}

#[derive(Clone, Debug)]
pub struct Choice<'a> {
    pub model: &'a Model,
    pub profile: &'a Profile,
    pub placement: Placement,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placement {
    Published,
    Ram { memory_mib: u64, host_memory_mib: u64 },
}

impl Placement {
    pub fn memory_mib(self, profile: &Profile) -> u64 {
        match self {
            Self::Published => profile.memory_mib,
            Self::Ram { memory_mib, .. } => memory_mib,
        }
    }

    pub fn host_memory_mib(self, profile: &Profile) -> u64 {
        match self {
            Self::Published => profile.host_memory_mib.unwrap_or(0),
            Self::Ram { host_memory_mib, .. } => host_memory_mib,
        }
    }

    pub fn gpu_layers(self, profile: &Profile, default: u32) -> u32 {
        if profile.accelerator == Accelerator::Cpu {
            return 0;
        }
        match self {
            Self::Published => profile.gpu_layers.unwrap_or(default),
            Self::Ram { .. } => 0,
        }
    }

    pub fn fits(self, profile: &Profile, budget_mib: u64, ram_budget_mib: u64) -> bool {
        self.memory_mib(profile) <= budget_mib && self.host_memory_mib(profile) <= ram_budget_mib
    }
}

impl Choice<'_> {
    pub fn memory_mib(&self) -> u64 {
        self.placement.memory_mib(self.profile)
    }

    pub fn host_memory_mib(&self) -> u64 {
        self.placement.host_memory_mib(self.profile)
    }
}

/// With weights on the host, reserve the profile's non-weight working set on
/// both sides, plus runtime overhead. A published zero-layer measurement is a
/// lower bound; the startup probe still has to qualify this estimate.
pub fn ram_fallback(model: &Model, profile: &Profile) -> Option<Placement> {
    if profile.accelerator != Accelerator::Cuda {
        return None;
    }
    let weights = model.weights.size_bytes.div_ceil(1 << 20);
    let working = profile.memory_mib.saturating_sub(weights);
    let mut memory_mib = working.max(1024).saturating_add(256);
    let mut host_memory_mib = weights.saturating_add(working).saturating_add(2048);
    for published in &model.profiles {
        if published.accelerator == Accelerator::Cuda
            && published.context_tokens == profile.context_tokens
            && published.slots() == profile.slots()
            && published.gpu_layers == Some(0)
        {
            memory_mib = memory_mib.max(published.memory_mib);
            host_memory_mib = host_memory_mib.max(published.host_memory_mib.unwrap_or(0));
        }
    }
    Some(Placement::Ram { memory_mib, host_memory_mib })
}

/// Parses repeated `model=value` flags.
pub fn parse_overrides(profiles: &[String], contexts: &[String]) -> Result<HashMap<String, Override>, String> {
    let mut overrides = HashMap::new();
    let mut insert = |model: &str, value: Override| match overrides.insert(model.to_string(), value) {
        Some(_) => Err(format!("model {model:?} has more than one --profile/--context override")),
        None => Ok(()),
    };
    for flag in profiles {
        let (model, profile) =
            flag.split_once('=').ok_or_else(|| format!("--profile {flag:?}: expected MODEL=PROFILE"))?;
        insert(model, Override::Profile(profile.to_string()))?;
    }
    for flag in contexts {
        let (model, tokens) =
            flag.split_once('=').ok_or_else(|| format!("--context {flag:?}: expected MODEL=TOKENS"))?;
        let tokens = tokens.parse().map_err(|_| format!("--context {flag:?}: TOKENS must be a number"))?;
        insert(model, Override::Context(tokens))?;
    }
    Ok(overrides)
}

/// Overrides first, then every other model's cheapest profile is reserved,
/// then models are upgraded in order to their most preferred profile that
/// still fits. A profile fits when its accelerator memory fits `budget_mib`
/// and, for offload profiles, the weights it keeps on the host fit
/// `ram_budget_mib`. Offloading is a fallback: any profile that runs fully on
/// the GPU is preferred (see `Profile::preference`). If no published placement
/// fits individually, CUDA may keep all weights in RAM at the smallest profile.
/// Overrides keep their exact profile and may also use that RAM placement.
pub fn plan<'a>(
    models: &[&'a Model],
    accelerator: Accelerator,
    budget_mib: u64,
    ram_budget_mib: u64,
    overrides: &HashMap<String, Override>,
) -> Result<Vec<Choice<'a>>, String> {
    for name in overrides.keys() {
        if !models.iter().any(|model| &model.id == name) {
            return Err(format!("override for {name:?}, which is not selected"));
        }
    }
    let host = |profile: &Profile| profile.host_memory_mib.unwrap_or(0);
    let mut choices = Vec::new();
    let mut fixed = Vec::new();
    for model in models {
        let candidates = model.profiles_for(accelerator);
        if candidates.is_empty() {
            return Err(format!("{} has no {accelerator} profile in the catalog", model.id));
        }
        let forced = overrides.contains_key(&model.id);
        let mut profile = match overrides.get(&model.id) {
            Some(Override::Profile(id)) => *candidates.iter().find(|profile| &profile.id == id).ok_or_else(|| {
                format!("{}: no {accelerator} profile {id:?} (available: {})", model.id, ids(&candidates))
            })?,
            // the best profile with exactly that context
            Some(Override::Context(tokens)) => {
                *candidates.iter().rev().find(|profile| profile.context_tokens == *tokens).ok_or_else(|| {
                    format!(
                        "{}: no {accelerator} profile with context {tokens} (available: {})",
                        model.id,
                        ids(&candidates)
                    )
                })?
            }
            // the least accelerator memory, preferring ones whose host memory fits
            None => *candidates
                .iter()
                .filter(|profile| host(profile) <= ram_budget_mib)
                .chain(candidates.iter())
                .min_by_key(|profile| (host(profile) > ram_budget_mib, profile.memory_mib))
                .expect("candidates is not empty"),
        };
        let mut placement = Placement::Published;
        let fallback = if forced {
            profile.memory_mib > budget_mib
        } else {
            !candidates.iter().any(|profile| Placement::Published.fits(profile, budget_mib, ram_budget_mib))
        };
        if fallback {
            let smallest = if forced {
                profile
            } else {
                *candidates
                    .iter()
                    .min_by_key(|profile| (profile.context_tokens, profile.slots(), profile.memory_mib))
                    .expect("candidates is not empty")
            };
            if let Some(ram) = ram_fallback(model, smallest)
                && ram.fits(smallest, budget_mib, ram_budget_mib)
            {
                profile = smallest;
                placement = ram;
            }
        }
        fixed.push(forced || placement != Placement::Published);
        choices.push(Choice { model, profile, placement });
    }

    let vram = |choices: &[Choice]| choices.iter().fold(0u64, |sum, choice| sum.saturating_add(choice.memory_mib()));
    let ram =
        |choices: &[Choice]| choices.iter().fold(0u64, |sum, choice| sum.saturating_add(choice.host_memory_mib()));
    if vram(&choices) > budget_mib || ram(&choices) > ram_budget_mib {
        let wanted: Vec<_> = choices.iter().map(describe).collect();
        return Err(format!(
            "the selected models do not fit: {} need {} MiB of accelerator memory and {} MiB of system RAM \
             together, but the budgets are {budget_mib} MiB and {ram_budget_mib} MiB",
            wanted.join(" + "),
            vram(&choices),
            ram(&choices)
        ));
    }

    for index in 0..choices.len() {
        if fixed[index] {
            continue;
        }
        let vram_room = budget_mib - (vram(&choices) - choices[index].memory_mib());
        let ram_room = ram_budget_mib - (ram(&choices) - choices[index].host_memory_mib());
        if let Some(best) = choices[index]
            .model
            .profiles_for(accelerator)
            .into_iter()
            .rev()
            .find(|profile| profile.memory_mib <= vram_room && host(profile) <= ram_room)
        {
            choices[index].profile = best;
        }
    }
    Ok(choices)
}

fn describe(choice: &Choice) -> String {
    let model = choice.model;
    let profile = choice.profile;
    match choice.placement {
        Placement::Ram { memory_mib, host_memory_mib } => format!(
            "{} {} ({memory_mib} MiB + {host_memory_mib} MiB RAM, automatic RAM fallback estimate)",
            model.id, profile.id
        ),
        Placement::Published => match profile.host_memory_mib {
            Some(host) => format!("{} {} ({} MiB + {host} MiB RAM)", model.id, profile.id, profile.memory_mib),
            None => format!("{} {} ({} MiB)", model.id, profile.id, profile.memory_mib),
        },
    }
}

fn ids(profiles: &[&Profile]) -> String {
    profiles.iter().map(|profile| profile.id.as_str()).collect::<Vec<_>>().join(", ")
}

/// Benchmarks prefer fitting full-GPU profiles unless placements are explicitly
/// selected. Offload follows when none fits; RAM fallback is the last resort.
pub fn benchmark_choices<'a>(
    model: &'a Model,
    accelerator: Accelerator,
    budget_mib: u64,
    ram_budget_mib: u64,
    override_: Option<&Override>,
) -> Result<Vec<Choice<'a>>, String> {
    let profiles = model.profiles_for(accelerator);
    let mut choices: Vec<_> = profiles
        .iter()
        .filter(|profile| Placement::Published.fits(profile, budget_mib, ram_budget_mib))
        .map(|&profile| Choice { model, profile, placement: Placement::Published })
        .collect();
    match override_ {
        Some(Override::Profile(id)) => {
            let overrides = HashMap::from([(model.id.clone(), Override::Profile(id.clone()))]);
            return plan(&[model], accelerator, budget_mib, ram_budget_mib, &overrides);
        }
        Some(Override::Context(tokens)) => {
            choices.retain(|choice| choice.profile.context_tokens == *tokens);
            if choices.is_empty() {
                let overrides = HashMap::from([(model.id.clone(), Override::Context(*tokens))]);
                return plan(&[model], accelerator, budget_mib, ram_budget_mib, &overrides);
            }
        }
        None if choices.is_empty() => return plan(&[model], accelerator, budget_mib, ram_budget_mib, &HashMap::new()),
        None => {
            if choices.iter().any(|choice| !choice.profile.offloaded()) {
                choices.retain(|choice| !choice.profile.offloaded());
            }
        }
    }
    choices.sort_by_key(|choice| (choice.profile.context_tokens, choice.profile.slots(), choice.profile.preference()));
    Ok(choices)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pool_protocol::Catalog;

    fn catalog() -> Catalog {
        Catalog::parse(include_str!("../../../tests/fixtures/all-models.json")).unwrap()
    }

    #[test]
    fn both_models_on_a_16_gb_card() {
        let catalog = catalog();
        let models = [catalog.model("clef").unwrap(), catalog.model("qwen2.5-1.5b-instruct").unwrap()];
        let choices = plan(&models, Accelerator::Cuda, 14888 - 256, 20000, &HashMap::new()).unwrap();
        let picked: Vec<_> = choices.iter().map(|choice| choice.profile.id.as_str()).collect();
        assert_eq!(picked, ["cuda-8192", "cuda-16384"]);
    }

    #[test]
    fn reports_conflicts_and_honors_overrides() {
        let catalog = catalog();
        let models = [catalog.model("clef").unwrap(), catalog.model("qwen2.5-1.5b-instruct").unwrap()];
        assert!(plan(&models, Accelerator::Cuda, 1000, 0, &HashMap::new()).unwrap_err().contains("do not fit"));
        let overrides = parse_overrides(&["clef=cuda-4096".into()], &["qwen2.5-1.5b-instruct=8192".into()]).unwrap();
        let choices = plan(&models, Accelerator::Cuda, 14632, 20000, &overrides).unwrap();
        assert_eq!(choices[0].profile.id, "cuda-4096");
        assert_eq!(choices[1].profile.id, "cuda-8192");
        let bad = parse_overrides(&[], &["clef=5000".into()]).unwrap();
        assert!(plan(&models, Accelerator::Cuda, 14632, 20000, &bad).is_err());
    }

    #[test]
    fn offloads_only_when_the_model_does_not_fit_on_the_gpu() {
        let catalog = catalog();
        let clef = [catalog.model("clef").unwrap()];
        // 16 GB card: everything on the GPU, no RAM needed
        let full = plan(&clef, Accelerator::Cuda, 14632, 20000, &HashMap::new()).unwrap();
        assert!(!full[0].profile.offloaded(), "{}", full[0].profile.id);
        // 8 GB card with plenty of RAM: offload, the best context that fits
        let eight = plan(&clef, Accelerator::Cuda, 7800, 20000, &HashMap::new()).unwrap();
        assert!(eight[0].profile.offloaded(), "{}", eight[0].profile.id);
        assert!(eight[0].profile.memory_mib <= 7800);
        // 8 GB card without RAM to spare: nothing fits
        assert!(plan(&clef, Accelerator::Cuda, 7800, 1000, &HashMap::new()).is_err());
    }

    #[test]
    fn a_4_gb_laptop_gpu_with_the_desktop_on_it_gets_a_short_context() {
        // RTX 3050 Ti Laptop: 1784 MiB free, 18.5 GB of RAM available
        let catalog = catalog();
        let clef = [catalog.model("clef").unwrap()];
        let choices = plan(&clef, Accelerator::Cuda, 1784 - 256, 18551 - 2048, &HashMap::new()).unwrap();
        assert_eq!(choices[0].profile.id, "cuda-2048-gpu0");
        assert_eq!(choices[0].placement, Placement::Published);
    }

    #[test]
    fn h2o_on_a_4_gb_gpu_with_2_5_gb_free_uses_ram() {
        let catalog = catalog();
        let h2o = catalog.model("h2o-q8").unwrap();
        let choices = plan(&[h2o], Accelerator::Cuda, 2560 - 256, 19000, &HashMap::new()).unwrap();
        let choice = &choices[0];
        assert_eq!(choice.profile.id, "cuda-8192");
        assert_eq!(choice.profile.context_tokens, 8192);
        assert_eq!(choice.profile.slots(), 1);
        assert_eq!(choice.placement, Placement::Ram { memory_mib: 1280, host_memory_mib: 7168 });
        assert_eq!(choice.placement.gpu_layers(choice.profile, h2o.execution.gpu_layers), 0);
        assert_eq!(choice.profile.memory_mib, 5120);
        assert_eq!(choice.profile.gpu_layers, None);
        assert!(std::ptr::eq(choice.profile, h2o.profile("cuda-8192").unwrap()));
        crate::setup::print_plan(&choices);
    }

    #[test]
    fn ram_fallback_needs_both_memory_budgets() {
        let catalog = catalog();
        let h2o = [catalog.model("h2o-q8").unwrap()];
        assert!(plan(&h2o, Accelerator::Cuda, 1280, 7168, &HashMap::new()).is_ok());
        assert!(plan(&h2o, Accelerator::Cuda, 1279, 7168, &HashMap::new()).is_err());
        assert!(plan(&h2o, Accelerator::Cuda, 1280, 7167, &HashMap::new()).is_err());
        let full = plan(&h2o, Accelerator::Cuda, 9216, 20000, &HashMap::new()).unwrap();
        assert_eq!(full[0].placement, Placement::Published);
    }

    #[test]
    fn forced_profiles_and_contexts_keep_their_setting_in_ram() {
        let catalog = catalog();
        let h2o = [catalog.model("h2o-q8").unwrap()];
        let overrides = parse_overrides(&["h2o-q8=cuda-40960-x3".into()], &[]).unwrap();
        let forced = plan(&h2o, Accelerator::Cuda, 6000, 20000, &overrides).unwrap();
        assert_eq!(forced[0].profile.id, "cuda-40960-x3");
        assert_eq!(forced[0].profile.slots(), 3);
        assert_eq!(forced[0].placement, Placement::Ram { memory_mib: 5197, host_memory_mib: 11264 });
        assert!(plan(&h2o, Accelerator::Cuda, 2304, 20000, &overrides).is_err());

        let overrides = parse_overrides(&[], &["h2o-q8=40960".into()]).unwrap();
        let forced = plan(&h2o, Accelerator::Cuda, 10000, 20000, &overrides).unwrap();
        assert_eq!(forced[0].profile.context_tokens, 40960);
        assert_eq!(forced[0].profile.id, "cuda-40960-x6");
        assert_ne!(forced[0].placement, Placement::Published);

        let automatic = plan(&h2o, Accelerator::Cuda, 6000, 20000, &HashMap::new()).unwrap();
        assert_eq!(automatic[0].profile.id, "cuda-8192");
        assert_eq!(automatic[0].profile.slots(), 1);
    }

    #[test]
    fn combined_pressure_does_not_trigger_ram_fallback() {
        let catalog = catalog();
        let mut second = catalog.model("h2o-q8").unwrap().clone();
        second.id = "second".into();
        let h2o = catalog.model("h2o-q8").unwrap();
        let error = plan(&[h2o, &second], Accelerator::Cuda, 10000, 40000, &HashMap::new()).unwrap_err();
        assert!(error.contains("do not fit"), "{error}");
        assert!(!error.contains("automatic RAM fallback"), "{error}");

        // Both qualify individually for RAM, but their host reservations add up.
        assert!(plan(&[h2o, &second], Accelerator::Cuda, 3000, 14000, &HashMap::new()).is_err());
        let choices = plan(&[h2o, &second], Accelerator::Cuda, 3000, 15000, &HashMap::new()).unwrap();
        assert!(choices.iter().all(|choice| choice.placement != Placement::Published));
    }

    #[test]
    fn ram_fallback_is_cuda_only_and_supports_chat() {
        let catalog = catalog();
        let chat = catalog.model("qwen2.5-1.5b-instruct").unwrap();
        let choices = plan(&[chat], Accelerator::Cuda, 1280, 10000, &HashMap::new()).unwrap();
        assert_eq!(choices[0].profile.id, "cuda-4096");
        assert_ne!(choices[0].placement, Placement::Published);
        assert!(plan(&[chat], Accelerator::Metal, 1280, 10000, &HashMap::new()).is_err());
        assert!(ram_fallback(chat, chat.profile("metal-4096").unwrap()).is_none());
    }

    #[test]
    fn ram_estimates_do_not_undercut_published_zero_layer_requirements() {
        let catalog = catalog();
        let clef = catalog.model("clef").unwrap();
        let profile = clef.profile("cuda-16384").unwrap();
        let ram = ram_fallback(clef, profile).unwrap();
        let published = clef.profile("cuda-16384-gpu0").unwrap();
        assert!(ram.memory_mib(profile) >= published.memory_mib);
        assert!(ram.host_memory_mib(profile) >= published.host_memory_mib.unwrap());

        let mut large = clef.clone();
        large.weights.size_bytes = u64::MAX;
        assert!(ram_fallback(&large, profile).unwrap().host_memory_mib(profile) > 1 << 40);
    }

    #[test]
    fn benchmarks_include_all_fitting_placements_and_respect_filters() {
        let catalog = catalog();
        let clef = catalog.model("clef").unwrap();
        let all = benchmark_choices(clef, Accelerator::Cuda, 20000, 40000, None).unwrap();
        assert_eq!(all.len(), 3);
        assert!(all.iter().all(|choice| !choice.profile.offloaded()));
        let offload = benchmark_choices(clef, Accelerator::Cuda, 7800, 40000, None).unwrap();
        assert!(!offload.is_empty());
        assert!(offload.iter().all(|choice| choice.profile.offloaded()));
        assert_eq!(
            offload.len(),
            clef.profiles_for(Accelerator::Cuda).iter().filter(|profile| profile.memory_mib <= 7800).count()
        );
        let selected = benchmark_choices(
            clef,
            Accelerator::Cuda,
            20000,
            40000,
            Some(&Override::Profile("cuda-4096-gpu16".into())),
        )
        .unwrap();
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].profile.id, "cuda-4096-gpu16");
        let short = benchmark_choices(clef, Accelerator::Cuda, 20000, 40000, Some(&Override::Context(4096))).unwrap();
        assert_eq!(short.len(), 5);
        assert!(short.iter().all(|choice| choice.profile.context_tokens == 4096));
        let full = benchmark_choices(clef, Accelerator::Cuda, 20000, 0, None).unwrap();
        assert_eq!(full.len(), 3);
        assert!(full.iter().all(|choice| !choice.profile.offloaded()));
        assert!(benchmark_choices(clef, Accelerator::Cuda, 20000, 40000, Some(&Override::Context(5000))).is_err());

        let h2o = catalog.model("h2o-q8").unwrap();
        let ram = benchmark_choices(h2o, Accelerator::Cuda, 2304, 19000, None).unwrap();
        assert_eq!(ram.len(), 1);
        assert_eq!(ram[0].profile.id, "cuda-8192");
        assert_ne!(ram[0].placement, Placement::Published);
        let slots = benchmark_choices(h2o, Accelerator::Cuda, 15000, 19000, Some(&Override::Context(40960))).unwrap();
        assert_eq!(slots.iter().map(|choice| choice.profile.slots()).collect::<Vec<_>>(), [3, 4, 6]);
        let forced =
            benchmark_choices(h2o, Accelerator::Cuda, 6000, 19000, Some(&Override::Profile("cuda-40960-x3".into())))
                .unwrap();
        assert_eq!(forced.len(), 1);
        assert_eq!(forced[0].profile.id, "cuda-40960-x3");
        assert_ne!(forced[0].placement, Placement::Published);
    }
}
