//! Miner-reported configuration, not cryptographic remote attestation.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::catalog::{Accelerator, Artifact};

pub const CHAT_JSON_CONTRACT: &str = "chat-json-v1";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatLaunch {
    pub context_tokens: u32,
    pub slots: u32,
    pub accelerator: Accelerator,
    pub gpu_layers: u32,
    pub batch_tokens: u32,
    pub microbatch_tokens: u32,
    pub cpu_threads: Option<usize>,
    pub flash_attention: bool,
    pub cache_type_k: String,
    pub cache_type_v: String,
    pub context_shift: bool,
    pub automatic_fit: bool,
    pub unified_memory: bool,
    pub jinja: bool,
    pub vision: bool,
    pub speculative: bool,
    /// Launch settings are known; actual tensor placement is not queried.
    pub placement_source: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeDescriptor {
    pub fingerprint: String,
    /// Fresh for each child. Reported for diagnostics, never a routing pin.
    pub instance_id: String,
    pub capability_contract: String,
    pub model: String,
    pub profile: String,
    pub artifact: Artifact,
    pub artifact_verification: String,
    pub llama_cpp_commit: String,
    pub build_info: String,
    /// The tokenizer is embedded in this exact GGUF and used by this build.
    pub tokenizer_identity: String,
    pub template_sha256: String,
    pub tool_template_sha256: Option<String>,
    pub bos_token: Option<String>,
    pub eos_token: Option<String>,
    pub launch: ChatLaunch,
    /// Effective /props generation parameters, without local paths or keys.
    pub generation_settings: Value,
}

impl RuntimeDescriptor {
    pub fn computed_fingerprint(&self) -> String {
        let mut value = serde_json::to_value(self).expect("runtime descriptor serializes");
        let object = value.as_object_mut().expect("descriptor is an object");
        object.remove("fingerprint");
        object.remove("instance_id");
        digest(&value)
    }

    pub fn matches_pin(&self, pin: Option<&str>) -> bool {
        pin.is_none_or(|pin| pin == self.fingerprint)
    }
}

pub fn digest(value: &Value) -> String {
    format!("sha256:{}", crate::catalog::hex(&Sha256::digest(canonical_bytes(value))))
}

pub fn canonical_bytes(value: &Value) -> Vec<u8> {
    fn sorted(value: &Value) -> Value {
        match value {
            Value::Object(fields) => {
                let mut keys: Vec<_> = fields.keys().collect();
                keys.sort();
                Value::Object(keys.into_iter().map(|key| (key.clone(), sorted(&fields[key]))).collect())
            }
            Value::Array(items) => Value::Array(items.iter().map(sorted).collect()),
            _ => value.clone(),
        }
    }
    serde_json::to_vec(&sorted(value)).expect("JSON serializes")
}
