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

pub use types::*;
mod types;
mod validation;
use validation::{is_hex, is_identifier};

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
        let mut identity =
            format!("{}\n{}\n{}\n{}\n{}", model.id, model.weights.sha256, model.weights.size_bytes, runtime, backend);
        if model.execution.chat_json || model.artifact.is_some() {
            identity.push_str(&crate::runtime::digest(&serde_json::json!({
                "artifact": model.artifact, "execution": model.execution,
                "contract": crate::runtime::CHAT_JSON_CONTRACT,
            })));
        }
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
            model.artifact_problems(&at, &mut problems);
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

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHIPPED: &str = include_str!("../../../config/models.json");
    /// Every backend, for the tests that need models the pool no longer serves.
    const ALL_MODELS: &str = include_str!("../../../tests/fixtures/all-models.json");

    #[test]
    fn shipped_catalog_serves_h2o_and_opt_in_chat() {
        let catalog = Catalog::parse(SHIPPED).expect("config/models.json is valid");
        let ids: Vec<_> = catalog.models.iter().map(|model| model.id.as_str()).collect();
        assert_eq!(ids, ["h2o-q8", "qwen3.8-27b-ud-q3-k-xl"]);
        assert!(catalog.model("h2o").is_none());
        assert_eq!(catalog.default_models, ["h2o-q8"]);
        let all = Catalog::parse(ALL_MODELS).expect("tests/fixtures/all-models.json is valid");
        assert_eq!(all.model("clef").unwrap().capability(), Capability::Systemone);
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
    fn offload_profiles_need_layers_and_host_memory_on_cuda() {
        let mut value: serde_json::Value = serde_json::from_str(ALL_MODELS).unwrap();
        let profiles = value["models"][0]["profiles"].as_array_mut().unwrap();
        let mut offload = profiles[0].clone();
        offload["id"] = "cuda-4096-gpu3".into();
        offload["gpu_layers"] = 3.into();
        profiles.push(offload.clone());
        let error = Catalog::parse(&value.to_string()).unwrap_err().to_string();
        assert!(error.contains("host_memory_mib"), "{error}");

        let profiles = value["models"][0]["profiles"].as_array_mut().unwrap();
        profiles.last_mut().unwrap()["host_memory_mib"] = 7000.into();
        let catalog = Catalog::parse(&value.to_string()).unwrap();
        let ranked = catalog.models[0].profiles_for(Accelerator::Cuda);
        assert!(ranked.first().unwrap().offloaded(), "offload ranks below every full-GPU profile");
    }

    #[test]
    fn h2o_is_a_systemone_model_with_slots_on_its_big_profile() {
        let catalog = Catalog::parse(SHIPPED).unwrap();
        let h2o = catalog.model("h2o-q8").unwrap();
        assert_eq!(h2o.capability(), Capability::Systemone);
        assert_eq!(serde_json::to_string(&h2o.backend).unwrap(), "\"llama-server-h2o\"");
        assert_eq!(h2o.profile("cuda-40960-x3").unwrap().slots(), 3);
        assert_eq!(h2o.profile("cuda-8192").unwrap().slots(), 1);
        let ranked: Vec<_> = h2o.profiles_for(Accelerator::Cuda).iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ranked, ["cuda-8192", "cuda-40960-x3", "cuda-40960-x4", "cuda-40960-x6"]);

        let mut value: serde_json::Value = serde_json::from_str(ALL_MODELS).unwrap();
        value["models"][0]["profiles"][0]["parallel_slots"] = 2.into();
        let error = Catalog::parse(&value.to_string()).unwrap_err().to_string();
        assert!(error.contains("parallel_slots"), "{error}");
    }

    #[test]
    fn model_revision_ignores_profiles() {
        let catalog = Catalog::parse(ALL_MODELS).unwrap();
        let model = catalog.model("clef").unwrap().clone();
        let mut fewer = model.clone();
        fewer.profiles.truncate(1);
        assert_eq!(catalog.model_revision(&model), catalog.model_revision(&fewer));
    }
}
