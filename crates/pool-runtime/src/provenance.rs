//! Read the pinned server's live configuration. Trust is miner-reported.

use std::path::Path;

use pool_protocol::{
    Model,
    catalog::hex,
    runtime::{CHAT_JSON_CONTRACT, ChatLaunch, RuntimeDescriptor},
};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{LaunchSpec, LocalClient, manifest};

pub async fn describe(
    model: &Model,
    profile: &pool_protocol::Profile,
    launch: &LaunchSpec,
    client: &LocalClient,
    instance: &str,
) -> Result<RuntimeDescriptor, String> {
    let props = client.props().await.map_err(|e| e.message)?;
    let artifact = model.artifact.clone().ok_or("missing artifact pin")?;
    let commit = manifest::embedded().runtime(&model.runtime).ok_or("unknown pinned runtime")?.llama_cpp_commit.clone();
    let text = |key: &str| {
        props
            .get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| format!("runtime metadata missing {key}"))
    };
    let build_info = text("build_info")?;
    let template = text("chat_template")?;
    let filename =
        props.get("model_path").and_then(Value::as_str).and_then(|p| Path::new(p).file_name()).and_then(|p| p.to_str());
    if filename != Some(artifact.filename.as_str())
        || !build_info.contains(&commit[..7])
        || props.pointer("/default_generation_settings/n_ctx").and_then(Value::as_u64)
            != Some(u64::from(launch.context_tokens))
        || props.get("total_slots").and_then(Value::as_u64) != Some(1)
        || props.pointer("/modalities/vision").and_then(Value::as_bool) != Some(false)
    {
        return Err("live runtime model/build/context/slots/modalities disagree with launch".into());
    }
    let hash = |text: &str| format!("sha256:{}", hex(&Sha256::digest(text.as_bytes())));
    let mut descriptor = RuntimeDescriptor {
        fingerprint: String::new(),
        instance_id: instance.into(),
        capability_contract: CHAT_JSON_CONTRACT.into(),
        model: model.id.clone(),
        profile: profile.id.clone(),
        artifact,
        artifact_verification: "miner_local_sha256_not_remote_attested".into(),
        llama_cpp_commit: commit.clone(),
        build_info,
        tokenizer_identity: format!("gguf-sha256:{};llama.cpp:{commit}", model.weights.sha256),
        template_sha256: hash(&template),
        tool_template_sha256: props.get("chat_template_tool_use").and_then(Value::as_str).map(hash),
        bos_token: props.get("bos_token").and_then(Value::as_str).map(str::to_owned),
        eos_token: props.get("eos_token").and_then(Value::as_str).map(str::to_owned),
        launch: ChatLaunch {
            context_tokens: launch.context_tokens,
            slots: 1,
            accelerator: launch.accelerator,
            gpu_layers: launch.gpu_layers,
            batch_tokens: launch.batch_tokens.unwrap_or(512),
            microbatch_tokens: launch.microbatch_tokens.unwrap_or(512),
            cpu_threads: launch.cpu_threads,
            flash_attention: true,
            cache_type_k: "f16".into(),
            cache_type_v: "f16".into(),
            context_shift: false,
            automatic_fit: false,
            unified_memory: false,
            jinja: true,
            vision: false,
            speculative: false,
            placement_source: "explicit_launch_not_measured".into(),
        },
        generation_settings: props
            .pointer("/default_generation_settings/params")
            .cloned()
            .ok_or("missing runtime sampler settings")?,
    };
    descriptor.fingerprint = descriptor.computed_fingerprint();
    Ok(descriptor)
}
