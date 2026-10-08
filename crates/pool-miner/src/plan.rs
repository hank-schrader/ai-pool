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

/// Overrides first, then every other model's smallest profile is reserved,
/// then models are upgraded in order to the largest profile that still fits.
pub fn plan<'a>(
    models: &[&'a Model],
    accelerator: Accelerator,
    budget_mib: u64,
    overrides: &HashMap<String, Override>,
) -> Result<Vec<Choice<'a>>, String> {
    for name in overrides.keys() {
        if !models.iter().any(|model| &model.id == name) {
            return Err(format!("override for {name:?}, which is not selected"));
        }
    }
    let mut choices = Vec::new();
    let mut fixed = Vec::new();
    for model in models {
        let candidates = model.profiles_for(accelerator);
        if candidates.is_empty() {
            return Err(format!("{} has no {accelerator} profile in the catalog", model.id));
        }
        let profile = match overrides.get(&model.id) {
            Some(Override::Profile(id)) => *candidates.iter().find(|profile| &profile.id == id).ok_or_else(|| {
                format!("{}: no {accelerator} profile {id:?} (available: {})", model.id, ids(&candidates))
            })?,
            Some(Override::Context(tokens)) => {
                *candidates.iter().find(|profile| profile.context_tokens == *tokens).ok_or_else(|| {
                    format!(
                        "{}: no {accelerator} profile with context {tokens} (available: {})",
                        model.id,
                        ids(&candidates)
                    )
                })?
            }
            None => candidates[0],
        };
        fixed.push(overrides.contains_key(&model.id));
        choices.push(Choice { model, profile });
    }

    let total = |choices: &[Choice]| choices.iter().map(|choice| choice.profile.memory_mib).sum::<u64>();
    if total(&choices) > budget_mib {
        let wanted: Vec<_> = choices
            .iter()
            .map(|choice| format!("{} {} ({} MiB)", choice.model.id, choice.profile.id, choice.profile.memory_mib))
            .collect();
        return Err(format!(
            "the selected models do not fit: {} need {} MiB together, but the budget is {budget_mib} MiB",
            wanted.join(" + "),
            total(&choices)
        ));
    }

    for index in 0..choices.len() {
        if fixed[index] {
            continue;
        }
        let others = total(&choices) - choices[index].profile.memory_mib;
        let room = budget_mib - others;
        if let Some(best) =
            choices[index].model.profiles_for(accelerator).into_iter().rev().find(|profile| profile.memory_mib <= room)
        {
            choices[index].profile = best;
        }
    }
    Ok(choices)
}

fn ids(profiles: &[&Profile]) -> String {
    profiles.iter().map(|profile| profile.id.as_str()).collect::<Vec<_>>().join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use pool_protocol::Catalog;

    fn catalog() -> Catalog {
        Catalog::parse(include_str!("../../../config/models.json")).unwrap()
    }

    #[test]
    fn both_models_on_a_16_gb_card() {
        let catalog = catalog();
        let models = [catalog.model("clef").unwrap(), catalog.model("qwen2.5-1.5b-instruct").unwrap()];
        let choices = plan(&models, Accelerator::Cuda, 14888 - 256, &HashMap::new()).unwrap();
        let picked: Vec<_> = choices.iter().map(|choice| choice.profile.id.as_str()).collect();
        assert_eq!(picked, ["cuda-8192", "cuda-16384"]);
    }

    #[test]
    fn reports_conflicts_and_honors_overrides() {
        let catalog = catalog();
        let models = [catalog.model("clef").unwrap(), catalog.model("qwen2.5-1.5b-instruct").unwrap()];
        assert!(plan(&models, Accelerator::Cuda, 8000, &HashMap::new()).unwrap_err().contains("do not fit"));
        let overrides = parse_overrides(&["clef=cuda-4096".into()], &["qwen2.5-1.5b-instruct=8192".into()]).unwrap();
        let choices = plan(&models, Accelerator::Cuda, 14632, &overrides).unwrap();
        assert_eq!(choices[0].profile.id, "cuda-4096");
        assert_eq!(choices[1].profile.id, "cuda-8192");
        let bad = parse_overrides(&[], &["clef=5000".into()]).unwrap();
        assert!(plan(&models, Accelerator::Cuda, 14632, &bad).is_err());
    }
}
