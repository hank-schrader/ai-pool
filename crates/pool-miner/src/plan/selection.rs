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
    if !model.execution.allow_ram_fallback || profile.accelerator != Accelerator::Cuda {
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
