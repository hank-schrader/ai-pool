use super::*;
use pool_protocol::{Model, runtime::CHAT_JSON_CONTRACT};

pub(super) fn revision_mismatch(fingerprint: Option<&str>) -> ApiError {
    if fingerprint.is_none() {
        return ApiError::miner_failed("miner reported inconsistent model/runtime metadata");
    }
    ApiError::new(
        StatusCode::CONFLICT,
        "revision_mismatch",
        "the requested runtime fingerprint is unavailable or changed; preflight and recount",
    )
}

pub(super) fn unavailable(model: &str, fingerprint: Option<&str>) -> ApiError {
    if fingerprint.is_some() { revision_mismatch(fingerprint) } else { ApiError::no_miner(model) }
}

pub(super) fn runtime_matches(loaded: &LoadedModel, fingerprint: Option<&str>) -> bool {
    fingerprint.is_none_or(|pin| loaded.runtime.as_ref().is_some_and(|r| r.fingerprint == pin))
}

pub(super) fn valid_runtime(catalog: &Catalog, model: &Model, loaded: &LoadedModel) -> bool {
    if !model.execution.chat_json {
        return true;
    }
    let Some(runtime) = &loaded.runtime else { return false };
    let Some(profile) = model.profile(&loaded.profile) else { return false };
    runtime.capability_contract == CHAT_JSON_CONTRACT
        && runtime.model == loaded.model
        && runtime.profile == loaded.profile
        && Some(&runtime.artifact) == model.artifact.as_ref()
        && catalog.runtime(&model.runtime).is_some_and(|r| r.llama_cpp_commit == runtime.llama_cpp_commit)
        && runtime.fingerprint == runtime.computed_fingerprint()
        && !runtime.instance_id.is_empty()
        && runtime.instance_id.len() <= 128
        && runtime.artifact_verification == "miner_local_sha256_not_remote_attested"
        && runtime.launch.context_tokens == loaded.context_tokens
        && runtime.launch.slots == 1
        && runtime.launch.accelerator == profile.accelerator
        && runtime.launch.gpu_layers == profile.gpu_layers.unwrap_or(model.execution.gpu_layers)
        && runtime.launch.batch_tokens == model.execution.batch_tokens.unwrap_or(512)
        && runtime.launch.microbatch_tokens == model.execution.microbatch_tokens.unwrap_or(512)
        && runtime.launch.jinja
        && !runtime.launch.context_shift
        && !runtime.launch.automatic_fit
        && !runtime.launch.unified_memory
        && runtime.launch.flash_attention
        && runtime.launch.cache_type_k == "f16"
        && runtime.launch.cache_type_v == "f16"
        && !runtime.launch.vision
        && !runtime.launch.speculative
        && !runtime.template_sha256.is_empty()
        && !runtime.tokenizer_identity.is_empty()
}
