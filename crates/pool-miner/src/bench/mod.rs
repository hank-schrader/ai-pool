//! Local pool-shaped inference, one exact catalog profile at a time.

mod metrics;
mod workload;

use std::{future::Future, sync::Arc, time::Duration};

use futures_util::{StreamExt, stream::FuturesUnordered};
use pool_protocol::{Accelerator, Backend, Catalog};
use serde::Serialize;
use serde_json::Value;
use tokio::{sync::mpsc, time::Instant};
use tokio_util::sync::CancellationToken;

use crate::{
    Cli, connection, hardware,
    hosted::{Hosted, Ready},
    plan::{self, Choice, Placement},
    setup,
};
use metrics::{Metrics, Peaks};
use workload::{Generator, Record};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(150);
const SAMPLE_INTERVAL: Duration = Duration::from_millis(500);

type Completion = (Result<Value, String>, u64, Duration);

#[derive(Serialize)]
struct Row {
    model: String,
    profile: String,
    placement: &'static str,
    planned_accelerator_mib: u64,
    planned_ram_mib: u64,
    memory_source: &'static str,
    gpu_layers: u32,
    context_tokens: u32,
    slots: u32,
    jobs: u32,
    status: &'static str,
    error: Option<String>,
    warnings: Vec<String>,
    metrics: Metrics,
    peaks: Peaks,
}

impl Row {
    fn new(choice: &Choice<'_>, prefetch: Option<u32>) -> Self {
        Self {
            model: choice.model.id.clone(),
            profile: choice.profile.id.clone(),
            placement: match choice.placement {
                Placement::Ram { .. } => "ram_fallback",
                Placement::Published if choice.profile.accelerator == Accelerator::Cpu => "cpu",
                Placement::Published if choice.profile.offloaded() => "offload",
                _ => "gpu",
            },
            gpu_layers: choice.placement.gpu_layers(choice.profile, choice.model.execution.gpu_layers),
            planned_accelerator_mib: choice.memory_mib(),
            planned_ram_mib: choice.host_memory_mib(),
            memory_source: match (choice.placement, choice.profile.memory_source) {
                (Placement::Published, pool_protocol::catalog::MemorySource::Measured) => "measured",
                _ => "estimate",
            },
            context_tokens: choice.profile.context_tokens,
            slots: choice.profile.slots(),
            jobs: connection::job_slots(choice.model.backend, choice.profile.slots(), prefetch),
            status: "failed",
            error: None,
            warnings: Vec::new(),
            metrics: Metrics::default(),
            peaks: Peaks::default(),
        }
    }

    fn finish(&mut self) {
        if self.metrics.completed < 10 {
            self.warnings.push(format!("small sample: {} completed requests (<10)", self.metrics.completed));
        }
        if self.metrics.missing_prompt_usage > 0 || self.metrics.missing_generated_usage > 0 {
            self.warnings.push("some responses omitted token usage; affected rates are unavailable".into());
        }
        if self.error.is_none() && self.metrics.errors == 0 && self.metrics.cancelled == 0 && self.metrics.completed > 0
        {
            self.status = "ok";
        }
    }
}

#[derive(Serialize)]
struct Report {
    schema_version: u32,
    catalog_revision: String,
    hardware: pool_protocol::messages::Hardware,
    cpu: String,
    accelerator_budget_mib: u64,
    ram_budget_mib: u64,
    runtime_commits: std::collections::BTreeMap<String, String>,
    workload_version: u32,
    seed: u64,
    admission_seconds: u64,
    request_timeout_seconds: u64,
    memory_sample_interval_ms: u64,
    memory_scope: &'static str,
    profiles: Vec<Row>,
}

pub async fn run(cli: &Cli, duration: u64, seed: u64, json: bool) -> Result<(), String> {
    let (catalog, revision) = cli.load_catalog(false).await?;
    let machine = hardware::detect(cli.device)?;
    eprintln!("Accelerator: {}", machine.describe());
    let selected = cli.select(&catalog, &machine)?;
    if selected.is_empty() {
        return Err("no models selected".into());
    }
    let options = cli.options()?;
    for id in options.overrides.keys() {
        if !selected.iter().any(|model| &model.id == id) {
            return Err(format!("override for {id:?}, which is not selected"));
        }
    }
    let mut system = sysinfo::System::new();
    system.refresh_cpu_all();
    let mut report = Report {
        schema_version: 1,
        catalog_revision: revision,
        hardware: machine.hardware(),
        cpu: system.cpus().first().map_or(String::new(), |cpu| cpu.brand().to_owned()),
        accelerator_budget_mib: machine.budget_mib,
        ram_budget_mib: machine.ram_budget_mib,
        runtime_commits: catalog
            .runtimes
            .iter()
            .map(|runtime| (runtime.id.clone(), runtime.llama_cpp_commit.clone()))
            .collect(),
        workload_version: workload::VERSION,
        seed,
        admission_seconds: duration,
        request_timeout_seconds: REQUEST_TIMEOUT.as_secs(),
        memory_sample_interval_ms: SAMPLE_INTERVAL.as_millis() as u64,
        memory_scope: "sampled runtime PID peaks from readiness through warm-up and drain; benchmark RSS separate",
        profiles: Vec::new(),
    };
    let mut choices = Vec::new();
    for model in &selected {
        match plan::benchmark_choices(
            model,
            machine.accelerator,
            machine.budget_mib,
            machine.ram_budget_mib,
            options.overrides.get(&model.id),
        ) {
            Ok(found) => choices.extend(found),
            Err(error) => {
                report.profiles.push(Row {
                    model: model.id.clone(),
                    profile: "unavailable".into(),
                    placement: "unavailable",
                    planned_accelerator_mib: 0,
                    planned_ram_mib: 0,
                    memory_source: "unavailable",
                    gpu_layers: 0,
                    context_tokens: 0,
                    slots: 0,
                    jobs: 0,
                    status: "failed",
                    error: Some(error),
                    warnings: Vec::new(),
                    metrics: Metrics::default(),
                    peaks: Peaks::default(),
                });
            }
        }
    }
    if !choices.is_empty() {
        let downloads: Vec<_> =
            selected.iter().copied().filter(|model| choices.iter().any(|choice| choice.model.id == model.id)).collect();
        setup::preflight(&catalog, &downloads, &machine, &options)?;
        setup::confirm_downloads(&options.cache, &downloads, options.yes)?;
    }
    let cancelled = CancellationToken::new();
    let signal_token = cancelled.clone();
    let signal = tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        signal_token.cancel();
    });
    let mut prepared = std::collections::HashMap::new();
    for choice in &choices {
        if cancelled.is_cancelled() {
            break;
        }
        let mut row = Row::new(choice, cli.prefetch);
        if !prepared.contains_key(&choice.model.id) {
            let result = tokio::select! {
                result = setup::prepare(&catalog, choice.model, &machine, &options) => result,
                _ = cancelled.cancelled() => Err("benchmark cancelled".into()),
            };
            prepared.insert(choice.model.id.clone(), result);
        }
        let result = match &prepared[&choice.model.id] {
            Ok(prepared) => {
                profile(&catalog, choice, prepared, cli, &options, &cancelled, duration, seed, &mut row).await
            }
            Err(error) => Err(error.clone()),
        };
        if let Err(error) = result {
            row.error = Some(error);
        }
        row.finish();
        report.profiles.push(row);
    }
    signal.abort();
    let _ = signal.await;
    if json {
        eprint!("{}", table(&report.profiles));
        println!("{}", serde_json::to_string_pretty(&report).map_err(|error| error.to_string())?);
    } else {
        print!("{}", table(&report.profiles));
    }
    if cancelled.is_cancelled() {
        return Err("benchmark cancelled".into());
    }
    let failures = report.profiles.iter().filter(|row| row.status != "ok").count();
    if failures > 0 { Err(format!("{failures} benchmark profile(s) failed")) } else { Ok(()) }
}

#[allow(clippy::too_many_arguments)]
async fn profile(
    catalog: &Catalog,
    choice: &Choice<'_>,
    prepared: &setup::Prepared,
    cli: &Cli,
    options: &setup::Options,
    cancelled: &CancellationToken,
    duration: u64,
    seed: u64,
    row: &mut Row,
) -> Result<(), String> {
    let machine = hardware::detect(cli.device)?;
    if !choice.placement.fits(choice.profile, machine.budget_mib, machine.ram_budget_mib) {
        return Err("available memory changed; this placement no longer fits".into());
    }
    eprintln!(
        "Benchmarking {} {} ({}): {} jobs, {duration}s admissions + drain",
        row.model, row.profile, row.placement, row.jobs
    );
    setup::print_plan(std::slice::from_ref(choice));
    let running = tokio::select! {
        running = setup::start_exact(catalog, choice, prepared, &machine, options) => running?,
        _ = cancelled.cancelled() => return Err("benchmark cancelled".into()),
    };
    let result = tokio::select! {
        result = running.wait_ready(Duration::from_secs(600)) => result,
        _ = cancelled.cancelled() => Err("benchmark cancelled".into()),
    };
    if let Err(error) = result {
        running.stop().await;
        return Err(error);
    }
    let hosted = running.models[0].clone();
    let Some(ready) = hosted.ready() else {
        running.stop().await;
        return Err("runtime unavailable".into());
    };
    let interrupted = CancellationToken::new();
    let sampling_done = CancellationToken::new();
    let sample_stop = sampling_done.clone();
    let sample_interrupt = interrupted.clone();
    let sample_hosted = hosted.clone();
    let sample_ready = ready.clone();
    let device = machine.device().index;
    let cuda = machine.accelerator == Accelerator::Cuda;
    let sampler = tokio::spawn(async move {
        let uuid = if cuda {
            metrics::smi(&["--id", &device.to_string(), "--query-gpu=uuid", "--format=csv,noheader"]).await.ok()
        } else {
            None
        };
        let mut peaks = Peaks::default();
        let mut interval = tokio::time::interval(SAMPLE_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = sample_stop.cancelled() => break,
                _ = interval.tick() => {
                    if !sample_hosted.ready().is_some_and(|current| current.client.base_url() == sample_ready.client.base_url()) {
                        sample_interrupt.cancel();
                    }
                    peaks.sample(sample_ready.pid, uuid.as_deref()).await;
                }
            }
        }
        peaks
    });
    let (sender, mut records) = mpsc::channel(row.jobs as usize);
    let producer = tokio::spawn(produce(hosted.clone(), choice.profile.context_tokens, seed, sender));
    let result = exercise(
        &mut records,
        &ready,
        choice.model.backend,
        row,
        Duration::from_secs(duration),
        cancelled,
        &interrupted,
    )
    .await;
    producer.abort();
    let _ = producer.await;
    sampling_done.cancel();
    row.peaks = sampler.await.unwrap_or_default();
    running.stop().await;
    result
}

async fn produce(hosted: Arc<Hosted>, context: u32, seed: u64, sender: mpsc::Sender<Result<Record, String>>) {
    let mut generator = Generator::new(seed);
    loop {
        let body = generator.record(&hosted.model);
        let result = tokio::time::timeout(
            REQUEST_TIMEOUT,
            workload::fit(&hosted.model, context, body, |payload| {
                let hosted = hosted.clone();
                async move { hosted.count(&payload).await.map_err(|error| error.to_string()) }
            }),
        )
        .await
        .unwrap_or_else(|_| Err("benchmark token counting timed out".into()));
        let failed = result.is_err();
        if sender.send(result).await.is_err() || failed {
            return;
        }
    }
}

async fn infer(ready: Ready, record: Record) -> Result<Value, String> {
    if ready.h2o.is_some() || record.questions > 0 {
        ready.systemone(&record.payload).await
    } else {
        ready.client.chat(&record.payload).await
    }
    .map_err(|error| format!("{:?}", error.code))
}

#[allow(clippy::too_many_arguments)]
async fn exercise(
    records: &mut mpsc::Receiver<Result<Record, String>>,
    ready: &Ready,
    backend: Backend,
    row: &mut Row,
    duration: Duration,
    cancelled: &CancellationToken,
    interrupted: &CancellationToken,
) -> Result<(), String> {
    warm_up(records, |record| infer(ready.clone(), record), row.jobs, cancelled, interrupted).await?;
    let result = measure(
        records,
        |record| infer(ready.clone(), record),
        row.jobs,
        duration,
        REQUEST_TIMEOUT,
        backend,
        cancelled,
        interrupted,
    )
    .await;
    row.metrics = result.0;
    result.1
}

async fn warm_up<F, Fut>(
    records: &mut mpsc::Receiver<Result<Record, String>>,
    execute: F,
    jobs: u32,
    cancelled: &CancellationToken,
    interrupted: &CancellationToken,
) -> Result<(), String>
where
    F: Fn(Record) -> Fut,
    Fut: Future<Output = Result<Value, String>>,
{
    let mut warmup = FuturesUnordered::new();
    for _ in 0..jobs {
        let record = tokio::select! {
            record = tokio::time::timeout(REQUEST_TIMEOUT, records.recv()) => record.map_err(|_| "warm-up preparation timed out")?.ok_or("workload generator stopped")??,
            _ = cancelled.cancelled() => return Err("benchmark cancelled".into()),
            _ = interrupted.cancelled() => return Err("runtime interrupted".into()),
        };
        warmup.push(tokio::time::timeout(REQUEST_TIMEOUT, execute(record)));
    }
    loop {
        tokio::select! {
            reply = warmup.next() => match reply {
                Some(reply) => { reply.map_err(|_| "warm-up inference timed out")??; },
                None => break,
            },
            _ = cancelled.cancelled() => return Err("benchmark cancelled".into()),
            _ = interrupted.cancelled() => return Err("runtime interrupted".into()),
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn measure<F, Fut>(
    records: &mut mpsc::Receiver<Result<Record, String>>,
    execute: F,
    jobs: u32,
    duration: Duration,
    timeout: Duration,
    backend: Backend,
    cancelled: &CancellationToken,
    interrupted: &CancellationToken,
) -> (Metrics, Result<(), String>)
where
    F: Fn(Record) -> Fut,
    Fut: Future<Output = Result<Value, String>>,
{
    let started = Instant::now();
    let deadline = started + duration;
    let mut metrics = Metrics::default();
    let mut pending: FuturesUnordered<futures_util::future::LocalBoxFuture<'_, Completion>> = FuturesUnordered::new();
    let mut admitting = true;
    let mut result = Ok(());
    loop {
        if Instant::now() >= deadline {
            admitting = false;
        }
        if !admitting && pending.is_empty() {
            break;
        }
        tokio::select! {
            biased;
            _ = cancelled.cancelled() => { result = Err("benchmark cancelled".into()); break; },
            _ = interrupted.cancelled() => { result = Err("runtime interrupted; measurements stopped".into()); break; },
            _ = tokio::time::sleep_until(deadline), if admitting => admitting = false,
            reply = pending.next(), if !pending.is_empty() => {
                if let Some((reply, questions, latency)) = reply {
                    match reply {
                        Ok(body) => metrics.success(&body, questions, latency, backend),
                        Err(code) => metrics.error(&code),
                    }
                }
            },
            record = records.recv(), if admitting && pending.len() < jobs as usize => {
                match record {
                    Some(Ok(record)) => {
                        if Instant::now() >= deadline { admitting = false; continue; }
                        metrics.submitted += 1;
                        let questions = record.questions;
                        let future = execute(record);
                        pending.push(Box::pin(async move {
                            let started = Instant::now();
                            let reply = tokio::time::timeout(timeout, future).await.unwrap_or_else(|_| Err("timeout".into()));
                            (reply, questions, started.elapsed())
                        }));
                    }
                    Some(Err(error)) => { result = Err(error); admitting = false; },
                    None => { result = Err("workload generator stopped".into()); admitting = false; },
                }
            },
        }
    }
    metrics.cancelled = pending.len() as u64;
    metrics.finish(started.elapsed(), backend);
    (metrics, result)
}

fn table(rows: &[Row]) -> String {
    use std::fmt::Write;
    let mut text = String::from(
        "Model        Profile            Place Jobs  Req/s    Q/s    In/s  Out/s  p50 s  p95 s Err  VRAM   RSS\n",
    );
    let number =
        |value: Option<f64>, decimals: usize| value.map_or("—".into(), |value| format!("{value:.decimals$}"));
    for row in rows {
        let model = if row.model == "qwen2.5-1.5b-instruct" {
            "qwen2.5-1.5b".into()
        } else {
            row.model.chars().take(12).collect::<String>()
        };
        let placement = match row.placement {
            "ram_fallback" => "RAM",
            "offload" => "off",
            "gpu" => "GPU",
            "cpu" => "CPU",
            _ => "—",
        };
        let profile: String = row.profile.chars().take(18).collect();
        let _ = writeln!(
            text,
            "{model:<12} {profile:<18} {placement:>5} {:>4} {:>6.2} {:>6} {:>7} {:>6} {:>6} {:>6} {:>3} {:>5} {:>5}",
            row.jobs,
            row.metrics.requests_per_second,
            number(row.metrics.questions_per_second, 2),
            number(row.metrics.prompt_tokens_per_second, 0),
            number(row.metrics.generated_tokens_per_second, 0),
            number(row.metrics.p50_seconds, 1),
            number(row.metrics.p95_seconds, 1),
            row.metrics.errors,
            row.peaks.runtime_vram_mib.map_or("—".into(), |value| value.to_string()),
            row.peaks.runtime_rss_mib.map_or("—".into(), |value| value.to_string())
        );
        for warning in &row.warnings {
            let _ = writeln!(text, "  {} {}: WARNING: {warning}", row.model, row.profile);
        }
        if let Some(error) = &row.error {
            let _ = writeln!(text, "  {} {}: FAILED: {error}", row.model, row.profile);
        }
        if row.metrics.errors > 0 {
            let _ = writeln!(text, "  errors: {:?}", row.metrics.error_counts);
        }
    }
    text.push_str("RAM = automatic RAM fallback (estimate); off = published offload. VRAM/RSS: runtime PID MiB.\nRates include drain time; latencies exclude preparation and warm-up.\n");
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use serde_json::json;

    #[tokio::test]
    async fn cli_accepts_benchmark_flags_and_loads_offline_catalog() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/all-models.json");
        let cli = Cli::try_parse_from([
            "pool-miner",
            "bench",
            "--duration",
            "60",
            "--seed",
            "42",
            "--json",
            "--models",
            "clef,h2o-q8,qwen2.5-1.5b-instruct",
            "--catalog",
            path.to_str().unwrap(),
            "--pool",
            "not-a-url",
            "--context",
            "clef=4096",
        ])
        .unwrap();
        assert_eq!(cli.models.len(), 3);
        assert!(matches!(cli.command, Some(crate::Command::Bench { duration: 60, seed: 42, json: true })));
        assert!(cli.load_catalog(false).await.unwrap().0.model("h2o-q8").is_some());
        assert!(Cli::try_parse_from(["pool-miner", "bench", "--duration", "0"]).is_err());
    }
    use std::sync::atomic::{AtomicUsize, Ordering};

    async fn records() -> (mpsc::Sender<Result<Record, String>>, mpsc::Receiver<Result<Record, String>>) {
        let (sender, receiver) = mpsc::channel(32);
        for _ in 0..32 {
            sender.send(Ok(Record { payload: json!({}), questions: 4 })).await.unwrap();
        }
        (sender, receiver)
    }

    #[tokio::test(start_paused = true)]
    async fn fake_inference_sample_excludes_warmup_and_includes_drain() {
        let catalog = Catalog::parse(include_str!("../../../../tests/fixtures/all-models.json")).unwrap();
        let model = catalog.model("h2o-q8").unwrap();
        let choices = plan::benchmark_choices(model, Accelerator::Cuda, 2304, 19000, None).unwrap();
        let mut row = Row::new(&choices[0], None);
        let (_sender, mut records) = records().await;
        let active = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let execute = |_record| async {
            let jobs = active.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(jobs, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_secs(45)).await;
            active.fetch_sub(1, Ordering::SeqCst);
            Ok(json!({"answers": {"q0": {}, "q1": {}, "q2": {}, "q3": {}}, "usage": {"input_tokens": 4000}}))
        };
        let cancelled = CancellationToken::new();
        let interrupted = CancellationToken::new();
        let started = Instant::now();
        warm_up(&mut records, &execute, row.jobs, &cancelled, &interrupted).await.unwrap();
        let (metrics, result) = measure(
            &mut records,
            &execute,
            row.jobs,
            Duration::from_secs(60),
            REQUEST_TIMEOUT,
            model.backend,
            &cancelled,
            &interrupted,
        )
        .await;
        result.unwrap();
        assert_eq!(metrics.submitted, 4);
        assert_eq!(metrics.completed, 4);
        assert_eq!(metrics.questions, 16);
        assert_eq!(metrics.elapsed_seconds, 90.0);
        assert_eq!(metrics.p50_seconds, Some(45.0));
        assert_eq!(metrics.requests_per_second, 4.0 / 90.0);
        assert_eq!(started.elapsed(), Duration::from_secs(135));
        assert_eq!(peak.load(Ordering::SeqCst), 2);
        row.metrics = metrics;
        row.peaks.runtime_vram_mib = Some(470);
        row.peaks.runtime_rss_mib = Some(9000);
        row.peaks.benchmark_rss_mib = Some(35);
        row.finish();
        assert_eq!(row.status, "ok");
        assert_eq!(row.warnings.len(), 1);
        let report = Report {
            schema_version: 1,
            catalog_revision: "fake-catalog".into(),
            hardware: pool_protocol::messages::Hardware {
                os: "fake".into(),
                arch: "cpu-test".into(),
                accelerator: Accelerator::Cuda,
                devices: vec![],
            },
            cpu: "CPU-only fake inference".into(),
            accelerator_budget_mib: 2304,
            ram_budget_mib: 19000,
            runtime_commits: std::collections::BTreeMap::from([("llama-b11374".into(), "fake-runtime".into())]),
            workload_version: workload::VERSION,
            seed: 20261012,
            admission_seconds: 60,
            request_timeout_seconds: 150,
            memory_sample_interval_ms: 500,
            memory_scope: "fake PID samples",
            profiles: vec![row],
        };
        let printed = table(&report.profiles);
        assert!(printed.lines().next().unwrap().len() <= 120);
        assert!(printed.contains("small sample: 4 completed requests (<10)"));
        println!("{printed}");
        println!("{}", serde_json::to_string_pretty(&report).unwrap());
    }

    #[tokio::test(start_paused = true)]
    async fn fixed_workload_fake_inference_table() {
        let catalog = Catalog::parse(include_str!("../../../../tests/fixtures/all-models.json")).unwrap();
        let model = catalog.model("qwen2.5-1.5b-instruct").unwrap();
        let choices = plan::benchmark_choices(model, Accelerator::Cuda, 4000, 19000, None).unwrap();
        let mut rows = Vec::new();
        for choice in &choices {
            let mut row = Row::new(choice, None);
            let mut generator = Generator::new(20261012);
            let (sender, mut records) = mpsc::channel(16);
            for _ in 0..16 {
                let record = workload::fit(
                    model,
                    choice.profile.context_tokens,
                    generator.record(model),
                    |payload| async move { Ok(serde_json::to_vec(&payload).unwrap().len().div_ceil(4) as u32) },
                )
                .await
                .unwrap();
                sender.send(Ok(record)).await.unwrap();
            }
            let execute = |record: Record| async move {
                tokio::time::sleep(Duration::from_secs(10)).await;
                Ok(serde_json::json!({"usage": {
                    "prompt_tokens": serde_json::to_vec(&record.payload["messages"]).unwrap().len().div_ceil(4),
                    "completion_tokens": 32,
                }}))
            };
            let (metrics, result) = measure(
                &mut records,
                execute,
                1,
                Duration::from_secs(60),
                REQUEST_TIMEOUT,
                model.backend,
                &CancellationToken::new(),
                &CancellationToken::new(),
            )
            .await;
            result.unwrap();
            assert_eq!(metrics.completed, 6);
            assert_eq!(metrics.requests_per_second, 0.1);
            row.metrics = metrics;
            row.finish();
            rows.push(row);
        }
        println!("Workload version {} (fake inference, unmeasured memory):\n{}", workload::VERSION, table(&rows));
    }

    #[tokio::test(start_paused = true)]
    async fn drain_timeout_and_runtime_errors_are_counted() {
        let (_sender, mut records) = records().await;
        let (metrics, result) = measure(
            &mut records,
            |_| std::future::pending::<Result<Value, String>>(),
            1,
            Duration::from_secs(60),
            REQUEST_TIMEOUT,
            Backend::LlamaServerH2o,
            &CancellationToken::new(),
            &CancellationToken::new(),
        )
        .await;
        result.unwrap();
        assert_eq!(metrics.elapsed_seconds, 150.0);
        assert_eq!(metrics.submitted, 1);
        assert_eq!(metrics.errors, 1);
        assert_eq!(metrics.timed_out, 1);
        assert_eq!(metrics.completed, 0);

        let (_sender, mut records) = self::records().await;
        let (metrics, _) = measure(
            &mut records,
            |_| async {
                tokio::time::sleep(Duration::from_secs(40)).await;
                Err("RuntimeFailed".into())
            },
            1,
            Duration::from_secs(60),
            REQUEST_TIMEOUT,
            Backend::LlamaServerChat,
            &CancellationToken::new(),
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(metrics.errors, 2);
        assert_eq!(metrics.error_counts["RuntimeFailed"], 2);
        assert_eq!(metrics.elapsed_seconds, 80.0);
    }

    #[tokio::test(start_paused = true)]
    async fn interruption_cancels_pending_work_and_stops_admission() {
        let (_sender, mut records) = records().await;
        let interrupted = CancellationToken::new();
        let stop = interrupted.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(5)).await;
            stop.cancel();
        });
        let (metrics, result) = measure(
            &mut records,
            |_| std::future::pending::<Result<Value, String>>(),
            2,
            Duration::from_secs(60),
            REQUEST_TIMEOUT,
            Backend::LlamaServerH2o,
            &CancellationToken::new(),
            &interrupted,
        )
        .await;
        assert!(result.unwrap_err().contains("runtime interrupted"));
        assert_eq!(metrics.cancelled, 2);
        assert_eq!(metrics.submitted, 2);
        assert_eq!(metrics.elapsed_seconds, 5.0);
    }

    #[test]
    fn chat_usage_missing_samples_and_failures_serialize() {
        let catalog = Catalog::parse(include_str!("../../../../tests/fixtures/all-models.json")).unwrap();
        let chat = catalog.model("qwen2.5-1.5b-instruct").unwrap();
        let choices = plan::benchmark_choices(chat, Accelerator::Cuda, 4000, 19000, None).unwrap();
        let mut row = Row::new(&choices[0], Some(20));
        assert_eq!(row.jobs, 1);
        row.metrics.success(
            &json!({"usage": {"prompt_tokens": 200, "completion_tokens": 40}}),
            0,
            Duration::from_secs(2),
            chat.backend,
        );
        row.metrics.error("timeout");
        row.metrics.finish(Duration::from_secs(10), chat.backend);
        row.finish();
        assert_eq!(row.status, "failed");
        assert_eq!(row.metrics.generated_tokens_per_second, Some(4.0));
        assert_eq!(row.metrics.questions_per_second, None);
        let json = serde_json::to_value(&row).unwrap();
        assert!(json["peaks"]["runtime_vram_mib"].is_null());
        assert_eq!(json["metrics"]["timed_out"], 1);
        assert!(table(&[row]).contains("qwen2.5-1.5b"));
        assert_eq!(connection::job_slots(Backend::LlamaServerH2o, 3, None), 6);
        assert_eq!(connection::job_slots(Backend::LlamaServerH2o, 3, Some(0)), 3);
        assert_eq!(connection::job_slots(Backend::LlamaServerH2o, 3, Some(2)), 5);
        assert_eq!(connection::job_slots(Backend::LlamaServerSystemone, 1, Some(10)), 1);
    }
}
