//! Approved runtime artifacts, shipped inside the miner (`runtime/manifest.json`).
//! A pool's catalog names a runtime id; only this manifest decides which
//! executables that id may download.

use std::sync::OnceLock;

use pool_protocol::Accelerator;
use serde::Deserialize;

const EMBEDDED: &str = include_str!("../../../runtime/manifest.json");

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub manifest_version: u32,
    pub runtimes: Vec<RuntimeSpec>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSpec {
    pub id: String,
    pub llama_cpp_commit: String,
    pub release_tag: String,
    pub targets: Vec<TargetSpec>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetSpec {
    pub target: String,
    pub accelerator: Accelerator,
    /// Exercised end to end on real hardware; others need explicit opt-in.
    pub qualified: bool,
    pub server_binary: String,
    pub archives: Vec<Archive>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Archive {
    pub name: String,
    pub url: String,
    pub size_bytes: u64,
    pub sha256: String,
}

pub fn embedded() -> &'static Manifest {
    static MANIFEST: OnceLock<Manifest> = OnceLock::new();
    MANIFEST.get_or_init(|| serde_json::from_str(EMBEDDED).expect("runtime/manifest.json is valid"))
}

impl Manifest {
    pub fn runtime(&self, id: &str) -> Option<&RuntimeSpec> {
        self.runtimes.iter().find(|runtime| runtime.id == id)
    }
}

impl RuntimeSpec {
    pub fn target(&self, target: &str, accelerator: Accelerator) -> Option<&TargetSpec> {
        self.targets.iter().find(|spec| spec.target == target && spec.accelerator == accelerator)
    }
}

/// Rust target triple of this miner build, as used in the manifest.
pub fn host_target() -> &'static str {
    if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        "x86_64-unknown-linux-gnu"
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        "aarch64-unknown-linux-gnu"
    } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        "x86_64-pc-windows-msvc"
    } else if cfg!(all(target_os = "windows", target_arch = "aarch64")) {
        "aarch64-pc-windows-msvc"
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "aarch64-apple-darwin"
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        "x86_64-apple-darwin"
    } else {
        "unknown"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_manifest_matches_catalog_runtime() {
        let catalog = pool_protocol::Catalog::parse(include_str!("../../../config/models.json")).unwrap();
        for runtime in &catalog.runtimes {
            let spec = embedded().runtime(&runtime.id).expect("catalog runtime is in the manifest");
            assert_eq!(spec.llama_cpp_commit, runtime.llama_cpp_commit);
            for target in &spec.targets {
                for archive in &target.archives {
                    assert_eq!(archive.sha256.len(), 64);
                    assert!(archive.url.ends_with(&archive.name));
                }
            }
        }
        assert!(
            embedded().runtime("llama-b11374").unwrap().target("x86_64-unknown-linux-gnu", Accelerator::Cuda).is_some()
        );
    }
}
