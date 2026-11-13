use super::*;

#[derive(Clone, Debug)]
pub struct LaunchSpec {
    /// Log label, e.g. the model id.
    pub label: String,
    pub server: PathBuf,
    pub weights: PathBuf,
    pub backend: Backend,
    pub chat_json: bool,
    pub accelerator: Accelerator,
    /// Tokens per slot. For H2O this is the prompt limit; the server gets two more.
    pub context_tokens: u32,
    /// `--parallel` slots (H2O only); Clef and chat always run one.
    pub slots: u32,
    pub gpu_layers: u32,
    /// Chat only.
    pub batch_tokens: Option<u32>,
    pub microbatch_tokens: Option<u32>,
    /// Restricts the child to one GPU (`CUDA_VISIBLE_DEVICES`).
    pub cuda_device: Option<String>,
    /// CPU worker threads on ARM Linux; other accelerators use runtime defaults.
    pub cpu_threads: Option<usize>,
}

impl LaunchSpec {
    pub(super) fn args(&self, port: u16, api_key: &str) -> Vec<String> {
        let h2o = self.backend == Backend::LlamaServerH2o;
        let slots = if h2o { self.slots.max(1) } else { 1 };
        // The pinned server needs prompt < slot context and marks the final readout
        // truncated at prompt + 1 >= context; two reserved tokens keep the prompt
        // limit inclusive, as in refactor-tool's engine.
        let total_context = if h2o || self.backend == Backend::LlamaServerOpenjev {
            (self.context_tokens + 2) * slots
        } else {
            self.context_tokens
        };
        let mut args = vec![
            "-m".into(),
            self.weights.display().to_string(),
            "-ngl".into(),
            self.gpu_layers.to_string(),
            "-c".into(),
            total_context.to_string(),
            "--host".into(),
            "127.0.0.1".into(),
            "--port".into(),
            port.to_string(),
            "--parallel".into(),
            slots.to_string(),
            "--api-key".into(),
            api_key.into(),
        ];
        match self.backend {
            Backend::LlamaServerSystemone => {
                let context = self.context_tokens.to_string();
                args.extend(["-b".into(), context.clone(), "-ub".into(), context]);
            }
            Backend::LlamaServerOpenjev => {
                // Bidirectional attention must see a whole prompt per microbatch.
                // Keep placement explicit and avoid loading the optional vision tower.
                let context = total_context.to_string();
                args.extend(["-b".into(), context.clone(), "-ub".into(), context]);
                args.extend(
                    [
                        "--fit",
                        "off",
                        "--jinja",
                        "--no-mmproj",
                        "-fa",
                        "on",
                        "-ctk",
                        "f16",
                        "-ctv",
                        "f16",
                        "--no-context-shift",
                        "--cache-ram",
                        "0",
                        "--ctx-checkpoints",
                        "0",
                    ]
                    .map(String::from),
                );
            }
            // refactor-tool's engine.rs flags for H2O, except 2048-token prompt chunks, no RAM
            // prompt cache (5% more questions answered) and no context checkpoints (9% more
            // with one question per request; see docs/qualification/h2o-flags.md).
            // CPU uses smaller chunks to bound cancellation delay between decode
            // calls. Context, slots, placement and CPU threads also vary.
            Backend::LlamaServerH2o => {
                let batch = if self.accelerator == Accelerator::Cpu { "512" } else { "2048" };
                args.extend(
                    [
                        "-b",
                        batch,
                        "-ub",
                        batch,
                        "-fa",
                        "on",
                        "-ctk",
                        "f16",
                        "-ctv",
                        "f16",
                        "--no-context-shift",
                        "--cache-ram",
                        "0",
                        "--ctx-checkpoints",
                        "0",
                    ]
                    .map(String::from),
                );
            }
            Backend::LlamaServerChat => {
                args.extend([
                    "-b".into(),
                    self.batch_tokens.unwrap_or(512).to_string(),
                    "-ub".into(),
                    self.microbatch_tokens.unwrap_or(512).to_string(),
                    "--no-context-shift".into(),
                ]);
            }
        }
        if self.chat_json {
            args.extend(
                [
                    "--fit",
                    "off",
                    "--jinja",
                    "--reasoning",
                    "off",
                    "--reasoning-format",
                    "none",
                    "--no-mmproj",
                    "--spec-type",
                    "none",
                    "-fa",
                    "on",
                    "-ctk",
                    "f16",
                    "-ctv",
                    "f16",
                    "--cache-ram",
                    "0",
                    "--ctx-checkpoints",
                    "0",
                ]
                .map(String::from),
            );
        }
        if self.accelerator == Accelerator::Cpu {
            // Without mmap, repacked tensors replace their file-backed copy.
            // b11374 calls the former --no-mmap option --load-mode none.
            args.extend(["--device", "none", "--no-op-offload", "--load-mode", "none"].map(String::from));
            if let Some(threads) = self.cpu_threads {
                args.extend(["-t".into(), threads.to_string(), "-tb".into(), threads.to_string()]);
            }
        }
        args
    }
}
