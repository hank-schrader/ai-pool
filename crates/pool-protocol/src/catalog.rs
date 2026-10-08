//! The host-maintained model catalog (`config/models.json`).
//!
//! Every profile is one tested execution setting: these weights, this runtime,
//! this accelerator and context, needing this much accelerator memory.
//! Miners select only published profiles; they never derive a context from
//! spare memory.

use std::{collections::HashSet, fmt, path::Path};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Catalog {
    #[serde(rename = "$schema", default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    pub schema_version: u32,
    /// Models a miner preselects when the user does not choose.
    pub default_models: Vec<String>,
    pub runtimes: Vec<Runtime>,
    pub models: Vec<Model>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Runtime {
    /// Referenced by `Model::runtime`; resolved by the miner's runtime manifest.
    pub id: String,
    /// Full 40-character llama.cpp commit the runtime must report.
    pub llama_cpp_commit: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Model {
    /// API model name clients send in `model`.
    pub id: String,
    pub description: String,
    pub backend: Backend,
    pub runtime: String,
    pub weights: Weights,
    pub limits: Limits,
    #[serde(default)]
    pub execution: Execution,
    pub profiles: Vec<Profile>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Backend {
    /// Decision model on `/v1/systemone`; prompt must fit one microbatch.
    LlamaServerSystemone,
    /// Text chat on `/v1/chat/completions`.
    LlamaServerChat,
}

impl Backend {
    pub fn capability(self) -> Capability {
        match self {
            Backend::LlamaServerSystemone => Capability::Systemone,
            Backend::LlamaServerChat => Capability::ChatCompletions,
        }
    }

    pub fn streaming(self) -> bool {
        matches!(self, Backend::LlamaServerChat)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    Systemone,
    ChatCompletions,
}

impl fmt::Display for Capability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Capability::Systemone => "systemone",
            Capability::ChatCompletions => "chat_completions",
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Weights {
    pub filename: String,
    pub url: String,
    pub size_bytes: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub request_bytes: usize,
    /// systemone only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_questions: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_choice_options: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_score_levels: Option<usize>,
    /// chat only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_output_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
}

/// Fixed launch settings shared by every profile of a model. Context-dependent
/// settings live on the profile. Only allowlisted options exist; the catalog can
/// never inject arbitrary runtime arguments.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Execution {
    #[serde(default = "default_gpu_layers")]
    pub gpu_layers: u32,
    /// Chat only; systemone always uses batch = microbatch = context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub microbatch_tokens: Option<u32>,
}

fn default_gpu_layers() -> u32 {
    99
}

impl Default for Execution {
    fn default() -> Self {
        Self { gpu_layers: default_gpu_layers(), batch_tokens: None, microbatch_tokens: None }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Accelerator {
    Cuda,
    Metal,
}

impl fmt::Display for Accelerator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Accelerator::Cuda => "cuda",
            Accelerator::Metal => "metal",
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemorySource {
    /// Peak measured during qualification, plus headroom.
    Measured,
    /// Not yet measured on this accelerator.
    Estimate,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub id: String,
    pub accelerator: Accelerator,
    pub context_tokens: u32,
    /// Accelerator memory one resident process needs at this context:
    /// dedicated VRAM for CUDA, unified memory for Metal.
    pub memory_mib: u64,
    pub memory_source: MemorySource,
}

#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error("cannot read catalog {path}: {source}")]
    Read { path: String, source: std::io::Error },
    #[error("invalid catalog JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid catalog:\n  - {}", .0.join("\n  - "))]
    Invalid(Vec<String>),
}

impl Catalog {
    pub fn load(path: &Path) -> Result<Self, CatalogError> {
        let text = std::fs::read_to_string(path)
            .map_err(|source| CatalogError::Read { path: path.display().to_string(), source })?;
        Self::parse(&text)
    }

    pub fn parse(text: &str) -> Result<Self, CatalogError> {
        let catalog: Catalog = serde_json::from_str(text)?;
        let problems = catalog.problems();
        if problems.is_empty() { Ok(catalog) } else { Err(CatalogError::Invalid(problems)) }
    }

    pub fn model(&self, id: &str) -> Option<&Model> {
        self.models.iter().find(|model| model.id == id)
    }

    pub fn runtime(&self, id: &str) -> Option<&Runtime> {
        self.runtimes.iter().find(|runtime| runtime.id == id)
    }

    /// Content digest of the whole catalog; miners report it so the pool can
    /// detect a miner started against an older catalog.
    pub fn revision(&self) -> String {
        let canonical = serde_json::to_vec(self).expect("catalog serializes");
        format!("sha256:{}", hex(&Sha256::digest(canonical)))
    }

    /// Identity of what a model computes: weights, runtime and wire contract.
    /// Profiles of one model share it; they differ only in context and memory.
    pub fn model_revision(&self, model: &Model) -> String {
        let runtime = self.runtime(&model.runtime).map(|r| r.llama_cpp_commit.as_str()).unwrap_or("");
        let backend = serde_json::to_string(&model.backend).expect("backend serializes");
        let identity =
            format!("{}\n{}\n{}\n{}\n{}", model.id, model.weights.sha256, model.weights.size_bytes, runtime, backend);
        format!("sha256:{}", hex(&Sha256::digest(identity.as_bytes())))
    }

    fn problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        if self.schema_version != SCHEMA_VERSION {
            problems
                .push(format!("schema_version {} is not supported (expected {SCHEMA_VERSION})", self.schema_version));
        }

        let mut runtime_ids = HashSet::new();
        for runtime in &self.runtimes {
            if !runtime_ids.insert(runtime.id.as_str()) {
                problems.push(format!("runtime {:?} is defined twice", runtime.id));
            }
            if !is_hex(&runtime.llama_cpp_commit, 40) {
                problems.push(format!("runtime {:?}: llama_cpp_commit must be a full 40-character commit", runtime.id));
            }
        }

        let mut model_ids = HashSet::new();
        for model in &self.models {
            let at = format!("model {:?}", model.id);
            if !is_identifier(&model.id) {
                problems.push(format!("{at}: id must match [a-z0-9][a-z0-9._-]{{0,63}}"));
            }
            if !model_ids.insert(model.id.as_str()) {
                problems.push(format!("{at}: id is used twice"));
            }
            if model.description.trim().is_empty() {
                problems.push(format!("{at}: description is empty"));
            }
            if !runtime_ids.contains(model.runtime.as_str()) {
                problems.push(format!("{at}: runtime {:?} is not defined", model.runtime));
            }
            model.weights.problems(&at, &mut problems);
            model.limits_problems(&at, &mut problems);
            model.profile_problems(&at, &mut problems);
        }

        if self.default_models.is_empty() {
            problems.push("default_models is empty".into());
        }
        for id in &self.default_models {
            if !model_ids.contains(id.as_str()) {
                problems.push(format!("default_models: {id:?} is not a defined model"));
            }
        }
        problems
    }
}

impl Weights {
    fn problems(&self, at: &str, problems: &mut Vec<String>) {
        if self.filename.is_empty()
            || self.filename.contains(['/', '\\'])
            || self.filename.starts_with('.')
            || !self.filename.ends_with(".gguf")
        {
            problems.push(format!("{at}: weights.filename must be a plain *.gguf file name"));
        }
        let local_http =
            ["http://127.0.0.1", "http://localhost", "http://[::1]"].iter().any(|prefix| self.url.starts_with(prefix));
        if !self.url.starts_with("https://") && !local_http {
            problems.push(format!("{at}: weights.url must use https (plain http only for localhost)"));
        }
        if self.size_bytes == 0 {
            problems.push(format!("{at}: weights.size_bytes must be positive"));
        }
        if !is_hex(&self.sha256, 64) {
            problems.push(format!("{at}: weights.sha256 must be 64 lowercase hexadecimal characters"));
        }
    }
}

impl Model {
    pub fn capability(&self) -> Capability {
        self.backend.capability()
    }

    pub fn max_context_tokens(&self) -> u32 {
        self.profiles.iter().map(|profile| profile.context_tokens).max().unwrap_or(0)
    }

    pub fn profile(&self, id: &str) -> Option<&Profile> {
        self.profiles.iter().find(|profile| profile.id == id)
    }

    /// Profiles for one accelerator, smallest context first.
    pub fn profiles_for(&self, accelerator: Accelerator) -> Vec<&Profile> {
        let mut profiles: Vec<_> = self.profiles.iter().filter(|p| p.accelerator == accelerator).collect();
        profiles.sort_by_key(|profile| profile.context_tokens);
        profiles
    }

    fn limits_problems(&self, at: &str, problems: &mut Vec<String>) {
        let limits = &self.limits;
        if limits.request_bytes == 0 || limits.request_bytes > 16 << 20 {
            problems.push(format!("{at}: limits.request_bytes must be between 1 and 16 MiB"));
        }
        let systemone = [limits.max_questions, limits.max_choice_options, limits.max_score_levels];
        let chat = [limits.default_output_tokens, limits.max_output_tokens];
        match self.backend {
            Backend::LlamaServerSystemone => {
                if systemone.iter().any(Option::is_none) || chat.iter().any(Option::is_some) {
                    problems.push(format!(
                        "{at}: systemone limits need max_questions, max_choice_options and max_score_levels only"
                    ));
                    return;
                }
                if !(1..=255).contains(&limits.max_questions.unwrap()) {
                    problems.push(format!("{at}: limits.max_questions must be 1-255"));
                }
                if !(2..=255).contains(&limits.max_choice_options.unwrap()) {
                    problems.push(format!("{at}: limits.max_choice_options must be 2-255"));
                }
                if !(2..=10).contains(&limits.max_score_levels.unwrap()) {
                    problems.push(format!("{at}: limits.max_score_levels must be 2-10"));
                }
            }
            Backend::LlamaServerChat => {
                if chat.iter().any(Option::is_none) || systemone.iter().any(Option::is_some) {
                    problems.push(format!("{at}: chat limits need default_output_tokens and max_output_tokens only"));
                    return;
                }
                let (default, max) = (limits.default_output_tokens.unwrap(), limits.max_output_tokens.unwrap());
                if default == 0 || default > max {
                    problems.push(format!("{at}: limits need 0 < default_output_tokens <= max_output_tokens"));
                }
            }
        }
    }

    fn profile_problems(&self, at: &str, problems: &mut Vec<String>) {
        let execution = &self.execution;
        match self.backend {
            Backend::LlamaServerSystemone => {
                if execution.batch_tokens.is_some() || execution.microbatch_tokens.is_some() {
                    problems.push(format!("{at}: systemone models use batch = microbatch = context; remove them"));
                }
            }
            Backend::LlamaServerChat => {
                let (batch, micro) =
                    (execution.batch_tokens.unwrap_or(512), execution.microbatch_tokens.unwrap_or(512));
                if batch == 0 || micro == 0 || micro > batch {
                    problems.push(format!("{at}: execution needs 0 < microbatch_tokens <= batch_tokens"));
                }
            }
        }
        if self.profiles.is_empty() {
            problems.push(format!("{at}: needs at least one profile"));
        }
        let mut ids = HashSet::new();
        for profile in &self.profiles {
            if !is_identifier(&profile.id) {
                problems.push(format!("{at}: profile id {:?} must match [a-z0-9][a-z0-9._-]{{0,63}}", profile.id));
            }
            if !ids.insert(profile.id.as_str()) {
                problems.push(format!("{at}: profile {:?} is defined twice", profile.id));
            }
            if !(512..=262_144).contains(&profile.context_tokens) {
                problems.push(format!("{at}: profile {:?}: context_tokens must be 512-262144", profile.id));
            }
            if profile.memory_mib == 0 {
                problems.push(format!("{at}: profile {:?}: memory_mib must be positive", profile.id));
            }
        }
        for accelerator in [Accelerator::Cuda, Accelerator::Metal] {
            let contexts: Vec<_> = self.profiles_for(accelerator).iter().map(|p| p.context_tokens).collect();
            if contexts.windows(2).any(|pair| pair[0] == pair[1]) {
                problems.push(format!("{at}: two {accelerator} profiles have the same context"));
            }
        }
    }
}

fn is_identifier(value: &str) -> bool {
    let mut chars = value.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit())
        && value.len() <= 64
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
}

fn is_hex(value: &str, len: usize) -> bool {
    value.len() == len && value.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHIPPED: &str = include_str!("../../../config/models.json");

    #[test]
    fn shipped_catalog_is_valid() {
        let catalog = Catalog::parse(SHIPPED).expect("config/models.json is valid");
        assert!(catalog.model("clef").is_some());
        assert_eq!(catalog.model("clef").unwrap().capability(), Capability::Systemone);
    }

    #[test]
    fn rejects_unknown_fields_and_bad_values() {
        let mut value: serde_json::Value = serde_json::from_str(SHIPPED).unwrap();
        value["models"][0]["weights"]["sha256"] = "XYZ".into();
        value["models"][0]["profiles"][0]["context_tokens"] = 10.into();
        let error = Catalog::parse(&value.to_string()).unwrap_err().to_string();
        assert!(error.contains("sha256"), "{error}");
        assert!(error.contains("context_tokens"), "{error}");

        value["models"][0]["surprise"] = true.into();
        assert!(matches!(Catalog::parse(&value.to_string()), Err(CatalogError::Json(_))));
    }

    #[test]
    fn model_revision_ignores_profiles() {
        let catalog = Catalog::parse(SHIPPED).unwrap();
        let model = catalog.model("clef").unwrap().clone();
        let mut fewer = model.clone();
        fewer.profiles.truncate(1);
        assert_eq!(catalog.model_revision(&model), catalog.model_revision(&fewer));
    }
}
