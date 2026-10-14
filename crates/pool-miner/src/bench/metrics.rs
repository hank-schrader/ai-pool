use std::{collections::BTreeMap, time::Duration};

use pool_protocol::Backend;
use serde::Serialize;
use serde_json::Value;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

#[derive(Default, Serialize)]
pub struct Metrics {
    pub submitted: u64,
    pub completed: u64,
    pub errors: u64,
    pub timed_out: u64,
    pub cancelled: u64,
    pub questions: u64,
    pub prompt_tokens: u64,
    pub generated_tokens: u64,
    pub missing_prompt_usage: u64,
    pub missing_generated_usage: u64,
    pub elapsed_seconds: f64,
    pub requests_per_second: f64,
    pub questions_per_second: Option<f64>,
    pub prompt_tokens_per_second: Option<f64>,
    pub generated_tokens_per_second: Option<f64>,
    pub p50_seconds: Option<f64>,
    pub p95_seconds: Option<f64>,
    pub error_counts: BTreeMap<String, u64>,
    #[serde(skip)]
    latencies: Vec<f64>,
}

impl Metrics {
    pub fn success(&mut self, body: &Value, questions: u64, elapsed: Duration, backend: Backend) {
        self.completed += 1;
        self.questions += questions;
        self.latencies.push(elapsed.as_secs_f64());
        match body["usage"].get("input_tokens").or_else(|| body["usage"].get("prompt_tokens")).and_then(Value::as_u64) {
            Some(tokens) => self.prompt_tokens += tokens,
            None => self.missing_prompt_usage += 1,
        }
        if backend == Backend::LlamaServerChat {
            match body["usage"]["completion_tokens"].as_u64() {
                Some(tokens) => self.generated_tokens += tokens,
                None => self.missing_generated_usage += 1,
            }
        }
    }

    pub fn error(&mut self, code: &str) {
        self.errors += 1;
        *self.error_counts.entry(code.into()).or_default() += 1;
        if code == "timeout" {
            self.timed_out += 1;
        }
    }

    pub fn finish(&mut self, elapsed: Duration, backend: Backend) {
        self.elapsed_seconds = elapsed.as_secs_f64();
        let seconds = self.elapsed_seconds.max(f64::EPSILON);
        self.requests_per_second = self.completed as f64 / seconds;
        self.questions_per_second = (backend != Backend::LlamaServerChat).then_some(self.questions as f64 / seconds);
        self.prompt_tokens_per_second =
            (self.completed > 0 && self.missing_prompt_usage == 0).then_some(self.prompt_tokens as f64 / seconds);
        self.generated_tokens_per_second =
            (backend == Backend::LlamaServerChat && self.completed > 0 && self.missing_generated_usage == 0)
                .then_some(self.generated_tokens as f64 / seconds);
        self.latencies.sort_by(f64::total_cmp);
        let percentile = |percent: usize| {
            if self.latencies.is_empty() {
                None
            } else {
                Some(self.latencies[(self.latencies.len() * percent).div_ceil(100) - 1])
            }
        };
        self.p50_seconds = percentile(50);
        self.p95_seconds = percentile(95);
    }
}

#[derive(Default, Serialize)]
pub struct Peaks {
    pub runtime_vram_mib: Option<u64>,
    pub runtime_rss_mib: Option<u64>,
    pub benchmark_rss_mib: Option<u64>,
    pub unavailable: BTreeMap<String, String>,
}

impl Peaks {
    pub async fn sample(&mut self, pid: Option<u32>, gpu_uuid: Option<&str>) {
        let (runtime, benchmark) = tokio::task::spawn_blocking(move || {
            let mut system = System::new();
            let miner = Pid::from_u32(std::process::id());
            let mut pids = vec![miner];
            if let Some(pid) = pid {
                pids.push(Pid::from_u32(pid));
            }
            system.refresh_processes_specifics(
                ProcessesToUpdate::Some(&pids),
                true,
                ProcessRefreshKind::nothing().with_memory(),
            );
            let rss = |pid| system.process(pid).map(|process| process.memory().div_ceil(1 << 20));
            (pid.and_then(|pid| rss(Pid::from_u32(pid))), rss(miner))
        })
        .await
        .unwrap_or((None, None));
        update_peak(&mut self.runtime_rss_mib, runtime);
        update_peak(&mut self.benchmark_rss_mib, benchmark);
        if runtime.is_none() {
            self.unavailable.insert("runtime_rss_mib".into(), "runtime PID or RSS unavailable".into());
        }
        if let (Some(pid), Some(uuid)) = (pid, gpu_uuid) {
            match smi(&["--query-compute-apps=pid,gpu_uuid,used_gpu_memory", "--format=csv,noheader,nounits"]).await {
                Ok(text) => match process_vram(&text, pid, uuid) {
                    Some(memory) => update_peak(&mut self.runtime_vram_mib, Some(memory)),
                    None => {
                        self.unavailable.insert("runtime_vram_mib".into(), "process VRAM unavailable".into());
                    }
                },
                Err(error) => {
                    self.unavailable.insert("runtime_vram_mib".into(), error);
                }
            }
        } else {
            self.unavailable.insert("runtime_vram_mib".into(), "dedicated VRAM or runtime PID unavailable".into());
        }
    }
}

fn update_peak(peak: &mut Option<u64>, value: Option<u64>) {
    if let Some(value) = value {
        *peak = Some(peak.unwrap_or(0).max(value));
    }
}

pub async fn smi(args: &[&str]) -> Result<String, String> {
    let mut command = tokio::process::Command::new("nvidia-smi");
    command.args(args).kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(2), command.output())
        .await
        .map_err(|_| "nvidia-smi sampling timed out".to_string())?
        .map_err(|error| format!("nvidia-smi unavailable: {error}"))?;
    if !output.status.success() {
        return Err("nvidia-smi sampling failed".into());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn process_vram(text: &str, pid: u32, uuid: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        let fields: Vec<_> = line.split(',').map(str::trim).collect();
        let [process, gpu, memory] = fields.as_slice() else {
            return None;
        };
        (process.parse::<u32>().ok() == Some(pid) && *gpu == uuid).then(|| memory.parse().ok()).flatten()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attributes_vram_to_only_the_runtime_on_the_selected_gpu() {
        let text = "1, GPU-a, 8000\n2, GPU-b, 9000\n2, GPU-a, 470\n";
        assert_eq!(process_vram(text, 2, "GPU-a"), Some(470));
        assert_eq!(process_vram("2, GPU-a, [N/A]", 2, "GPU-a"), None);
        assert_eq!(process_vram(text, 3, "GPU-a"), None);
    }

    #[test]
    fn rates_percentiles_and_missing_usage() {
        let mut metrics = Metrics::default();
        for seconds in 1..=20 {
            metrics.success(
                &serde_json::json!({"usage": {"input_tokens": 100}}),
                4,
                Duration::from_secs(seconds),
                Backend::LlamaServerH2o,
            );
        }
        metrics.finish(Duration::from_secs(40), Backend::LlamaServerH2o);
        assert_eq!(metrics.requests_per_second, 0.5);
        assert_eq!(metrics.questions_per_second, Some(2.0));
        assert_eq!(metrics.prompt_tokens_per_second, Some(50.0));
        assert_eq!(metrics.p50_seconds, Some(10.0));
        assert_eq!(metrics.p95_seconds, Some(19.0));
        metrics.success(&Value::Null, 1, Duration::ZERO, Backend::LlamaServerH2o);
        metrics.finish(Duration::from_secs(40), Backend::LlamaServerH2o);
        assert_eq!(metrics.prompt_tokens_per_second, None);
    }
}
