use super::*;

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<Artifact>,
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
    /// H2O-Lightning decision model on `/v1/systemone`: one label-only forward
    /// per question, answered exactly like refactor-tool's local engine.
    #[serde(rename = "llama-server-h2o")]
    LlamaServerH2o,
}

impl Backend {
    pub fn capability(self) -> Capability {
        match self {
            Backend::LlamaServerSystemone | Backend::LlamaServerH2o => Capability::Systemone,
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

/// Immutable model installation identity; verified by the miner downloader.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub repository: String,
    pub revision: String,
    pub filename: String,
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
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub allow_ram_fallback: bool,
    /// Fixed text-only Jinja launch and runtime provenance contract.
    #[serde(default, skip_serializing_if = "is_false")]
    pub chat_json: bool,
}

fn yes() -> bool {
    true
}
fn is_true(value: &bool) -> bool {
    *value
}
fn is_false(value: &bool) -> bool {
    !*value
}

fn default_gpu_layers() -> u32 {
    99
}

impl Default for Execution {
    fn default() -> Self {
        Self {
            gpu_layers: default_gpu_layers(),
            batch_tokens: None,
            microbatch_tokens: None,
            allow_ram_fallback: true,
            chat_json: false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Accelerator {
    Cuda,
    Metal,
    Cpu,
}

impl fmt::Display for Accelerator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Accelerator::Cuda => "cuda",
            Accelerator::Metal => "metal",
            Accelerator::Cpu => "cpu",
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
    /// dedicated VRAM for CUDA, unified memory for Metal, system RAM for CPU.
    pub memory_mib: u64,
    pub memory_source: MemorySource,
    /// Offload profiles only: layers kept on the GPU; the rest of the weights
    /// stay in system RAM. Unset means every layer is on the accelerator;
    /// CPU profiles always keep all weights in system RAM.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu_layers: Option<u32>,
    /// Offload profiles only: system RAM the weights left on the host need.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_memory_mib: Option<u64>,
    /// llama-server `--parallel` slots, each with `context_tokens` of its own
    /// (H2O only; unset means 1). Slots share the weights and run that many
    /// questions at once; the miner may queue a few more in front of them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel_slots: Option<u32>,
}

impl Profile {
    /// llama-server slots this profile starts.
    pub fn slots(&self) -> u32 {
        self.parallel_slots.unwrap_or(1)
    }

    pub fn offloaded(&self) -> bool {
        self.gpu_layers.is_some()
    }

    /// Ordering of preference, best last: everything on the GPU beats any
    /// offload, then more context, then more layers on the GPU, then more slots.
    pub fn preference(&self) -> (bool, u32, u32, u32) {
        (!self.offloaded(), self.context_tokens, self.gpu_layers.unwrap_or(u32::MAX), self.slots())
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

    /// Profiles for one accelerator, least preferred first (see [`Profile::preference`]).
    pub fn profiles_for(&self, accelerator: Accelerator) -> Vec<&Profile> {
        let mut profiles: Vec<_> = self.profiles.iter().filter(|p| p.accelerator == accelerator).collect();
        profiles.sort_by_key(|profile| profile.preference());
        profiles
    }
}
