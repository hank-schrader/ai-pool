use super::*;

impl Weights {
    pub(super) fn problems(&self, at: &str, problems: &mut Vec<String>) {
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
    pub(super) fn artifact_problems(&self, at: &str, problems: &mut Vec<String>) {
        if self.execution.chat_json && (self.backend != Backend::LlamaServerChat || self.artifact.is_none()) {
            problems.push(format!("{at}: chat_json needs a chat backend and a complete artifact pin"));
        }
        if let Some(pin) = &self.artifact {
            let expected =
                format!("https://huggingface.co/{}/resolve/{}/{}", pin.repository, pin.revision, pin.filename);
            if pin.repository.split('/').count() != 2
                || pin
                    .repository
                    .split('/')
                    .any(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)))
                || !is_hex(&pin.revision, 40)
                || pin.filename != self.weights.filename
                || pin.sha256 != self.weights.sha256
                || pin.size_bytes != self.weights.size_bytes
                || expected != self.weights.url
            {
                problems.push(format!("{at}: artifact must match the weights and an immutable HF revision URL"));
            }
        }
    }

    pub(super) fn limits_problems(&self, at: &str, problems: &mut Vec<String>) {
        let limits = &self.limits;
        if limits.request_bytes == 0 || limits.request_bytes > 16 << 20 {
            problems.push(format!("{at}: limits.request_bytes must be between 1 and 16 MiB"));
        }
        let systemone = [limits.max_questions, limits.max_choice_options, limits.max_score_levels];
        let chat = [limits.default_output_tokens, limits.max_output_tokens];
        match self.backend {
            Backend::LlamaServerH2o => {
                if systemone.iter().any(Option::is_none) || chat.iter().any(Option::is_some) {
                    problems.push(format!(
                        "{at}: h2o limits need max_questions, max_choice_options and max_score_levels only"
                    ));
                    return;
                }
                if !(1..=255).contains(&limits.max_questions.unwrap()) {
                    problems.push(format!("{at}: limits.max_questions must be 1-255"));
                }
                // one label per option, A-P
                if !(1..=16).contains(&limits.max_choice_options.unwrap()) {
                    problems.push(format!("{at}: limits.max_choice_options must be 1-16 (labels A-P)"));
                }
                if !(1..=16).contains(&limits.max_score_levels.unwrap()) {
                    problems.push(format!("{at}: limits.max_score_levels must be 1-16 (labels A-P)"));
                }
            }
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

    pub(super) fn profile_problems(&self, at: &str, problems: &mut Vec<String>) {
        let execution = &self.execution;
        match self.backend {
            Backend::LlamaServerSystemone | Backend::LlamaServerH2o => {
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
            match profile.parallel_slots {
                None => {}
                Some(1..=8) if self.backend == Backend::LlamaServerH2o => {}
                Some(_) => problems.push(format!(
                    "{at}: profile {:?}: parallel_slots must be 1-8 and is only supported by h2o models",
                    profile.id
                )),
            }
            match (profile.gpu_layers, profile.host_memory_mib) {
                (None, None) => {}
                (Some(_), Some(host)) if host > 0 && profile.accelerator == Accelerator::Cuda => {}
                (Some(_), Some(_)) if profile.accelerator != Accelerator::Cuda => problems
                    .push(format!("{at}: profile {:?}: separate host memory is only supported on cuda", profile.id)),
                _ => problems.push(format!(
                    "{at}: profile {:?}: offload profiles need both gpu_layers and a positive host_memory_mib",
                    profile.id
                )),
            }
        }
        for accelerator in [Accelerator::Cuda, Accelerator::Metal, Accelerator::Cpu] {
            let keys: Vec<_> = self.profiles_for(accelerator).iter().map(|p| p.preference()).collect();
            if keys.windows(2).any(|pair| pair[0] == pair[1]) {
                problems.push(format!("{at}: two {accelerator} profiles have the same context, GPU layers and slots"));
            }
        }
    }
}

pub(super) fn is_identifier(value: &str) -> bool {
    let mut chars = value.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit())
        && value.len() <= 64
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
}

pub(super) fn is_hex(value: &str, len: usize) -> bool {
    value.len() == len && value.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
