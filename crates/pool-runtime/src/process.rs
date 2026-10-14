//! A managed `llama-server` child bound to loopback with a private API key.

use std::{
    collections::VecDeque,
    net::TcpListener,
    path::PathBuf,
    process::{ExitStatus, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use pool_protocol::Backend;
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, BufReader},
    process::Command,
    sync::{oneshot, watch},
};

use crate::{Error, LocalClient, Result};

const HEALTH_TIMEOUT: Duration = Duration::from_secs(300);
const LOG_TAIL: usize = 30;

#[derive(Clone, Debug)]
pub struct LaunchSpec {
    /// Log label, e.g. the model id.
    pub label: String,
    pub server: PathBuf,
    pub weights: PathBuf,
    pub backend: Backend,
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
}

impl LaunchSpec {
    fn args(&self, port: u16, api_key: &str) -> Vec<String> {
        let h2o = self.backend == Backend::LlamaServerH2o;
        let slots = if h2o { self.slots.max(1) } else { 1 };
        // The pinned server needs prompt < slot context and marks the final readout
        // truncated at prompt + 1 >= context; two reserved tokens keep the prompt
        // limit inclusive, as in refactor-tool's engine.
        let total_context = if h2o { (self.context_tokens + 2) * slots } else { self.context_tokens };
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
            // refactor-tool's engine.rs flags for H2O, except 2048-token prompt chunks and
            // no RAM prompt cache: 5% more questions answered (docs/qualification/h2o-flags.md).
            // Only the context, slots and -ngl vary.
            Backend::LlamaServerH2o => args.extend(
                [
                    "-b",
                    "2048",
                    "-ub",
                    "2048",
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
                    "8",
                ]
                .map(String::from),
            ),
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
        args
    }
}

/// Dropping it kills and reaps the child.
pub struct LlamaServer {
    client: LocalClient,
    exited: watch::Receiver<Option<String>>,
    kill: Option<oneshot::Sender<()>>,
    pid: Option<u32>,
}

impl LlamaServer {
    /// Starts the child and waits until `/health` answers 200.
    pub async fn start(spec: &LaunchSpec) -> Result<Self> {
        let mut last = None;
        // a free port can be taken between probing and binding; retry a few times
        for _ in 0..3 {
            match Self::start_once(spec).await {
                Ok(server) => return Ok(server),
                Err(error) => {
                    tracing::warn!("{}: llama-server failed to start: {error}", spec.label);
                    last = Some(error);
                }
            }
        }
        Err(last.unwrap())
    }

    async fn start_once(spec: &LaunchSpec) -> Result<Self> {
        let port = free_port()?;
        let api_key = uuid::Uuid::new_v4().simple().to_string();
        let mut command = Command::new(&spec.server);
        command
            .args(spec.args(port, &api_key))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(device) = &spec.cuda_device {
            command.env("CUDA_VISIBLE_DEVICES", device);
        }
        crate::requirements::library_path(&mut command, &spec.server)?;
        #[cfg(target_os = "linux")]
        unsafe {
            // die with the miner even if it is killed without running destructors
            command.pre_exec(|| {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::getppid() == 1 {
                    return Err(std::io::Error::other("parent already exited"));
                }
                Ok(())
            });
        }
        let mut child =
            command.spawn().map_err(|error| Error::msg(format!("cannot start {}: {error}", spec.server.display())))?;
        let pid = child.id();

        let tail = Arc::new(Mutex::new(VecDeque::with_capacity(LOG_TAIL)));
        forward_logs(child.stdout.take(), spec.label.clone(), tail.clone());
        forward_logs(child.stderr.take(), spec.label.clone(), tail.clone());

        let (exit_tx, exited) = watch::channel(None);
        let (kill, kill_rx) = oneshot::channel::<()>();
        let label = spec.label.clone();
        let wait_tail = tail.clone();
        tokio::spawn(async move {
            let status: std::io::Result<ExitStatus> = tokio::select! {
                status = child.wait() => status,
                _ = kill_rx => {
                    let _ = child.start_kill();
                    child.wait().await
                }
            };
            let reason = match status {
                Ok(status) => format!("exited with {status}"),
                Err(error) => format!("wait failed: {error}"),
            };
            tracing::debug!("{label}: llama-server {reason}");
            let tail: Vec<String> = wait_tail.lock().unwrap().iter().cloned().collect();
            let _ = exit_tx.send(Some(format!("{reason}; last output:\n{}", tail.join("\n"))));
        });

        let client = LocalClient::new(format!("http://127.0.0.1:{port}"), api_key);
        let mut server = Self { client, exited, kill: Some(kill), pid };
        let started = Instant::now();
        loop {
            if let Some(reason) = server.exit_reason() {
                return Err(Error::msg(format!("llama-server {reason}")));
            }
            if server.client.healthy().await {
                tracing::info!(
                    "{}: llama-server ready on port {port} in {:.1}s",
                    spec.label,
                    started.elapsed().as_secs_f32()
                );
                return Ok(server);
            }
            if started.elapsed() > HEALTH_TIMEOUT {
                server.stop();
                return Err(Error::msg(format!("llama-server not healthy after {}s", HEALTH_TIMEOUT.as_secs())));
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    pub fn client(&self) -> &LocalClient {
        &self.client
    }

    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// `Some(reason)` once the child has exited.
    pub fn exit_reason(&self) -> Option<String> {
        self.exited.borrow().clone()
    }

    /// Resolves when the child exits, with the reason.
    pub async fn wait_exit(&self) -> String {
        let mut exited = self.exited.clone();
        loop {
            if let Some(reason) = exited.borrow_and_update().clone() {
                return reason;
            }
            if exited.changed().await.is_err() {
                return "exit watcher stopped".into();
            }
        }
    }

    pub fn stop(&mut self) {
        if let Some(kill) = self.kill.take() {
            let _ = kill.send(());
        }
    }
}

impl Drop for LlamaServer {
    fn drop(&mut self) {
        self.stop();
    }
}

fn free_port() -> Result<u16> {
    Ok(TcpListener::bind("127.0.0.1:0")?.local_addr()?.port())
}

/// llama-server lines look like `0.01.047.026 I slot release: ...`; the level
/// letter decides the tracing level.
fn forward_logs(
    stream: Option<impl AsyncRead + Unpin + Send + 'static>,
    label: String,
    tail: Arc<Mutex<VecDeque<String>>>,
) {
    let Some(stream) = stream else { return };
    tokio::spawn(async move {
        let mut lines = BufReader::new(stream).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let level = line.split_whitespace().nth(1).unwrap_or_default();
            match level {
                "E" | "W" => tracing::warn!(target: "llama_server", "{label}: {line}"),
                _ => tracing::debug!(target: "llama_server", "{label}: {line}"),
            }
            let mut tail = tail.lock().unwrap();
            if tail.len() == LOG_TAIL {
                tail.pop_front();
            }
            tail.push_back(line);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(backend: Backend, slots: u32) -> LaunchSpec {
        LaunchSpec {
            label: "m".into(),
            server: "llama-server".into(),
            weights: "m.gguf".into(),
            backend,
            context_tokens: 40960,
            slots,
            gpu_layers: 99,
            batch_tokens: None,
            microbatch_tokens: None,
            cuda_device: None,
        }
    }

    fn value<'a>(args: &'a [String], flag: &str) -> &'a str {
        let at = args.iter().position(|arg| arg == flag).unwrap();
        &args[at + 1]
    }

    #[test]
    fn h2o_uses_pool_flags_with_two_reserved_tokens_per_slot() {
        let args = spec(Backend::LlamaServerH2o, 3).args(1, "key");
        assert_eq!(value(&args, "-c"), (40962 * 3).to_string());
        for (flag, expected) in [
            ("--parallel", "3"),
            ("-b", "2048"),
            ("-ub", "2048"),
            ("-fa", "on"),
            ("-ctk", "f16"),
            ("-ctv", "f16"),
            ("--cache-ram", "0"),
            ("--ctx-checkpoints", "8"),
        ] {
            assert_eq!(value(&args, flag), expected, "{flag}");
        }
        assert!(args.contains(&"--no-context-shift".to_string()));
    }

    #[test]
    fn clef_keeps_one_slot_and_the_plain_context() {
        let args = spec(Backend::LlamaServerSystemone, 3).args(1, "key");
        assert_eq!(value(&args, "-c"), "40960");
        assert_eq!(value(&args, "--parallel"), "1");
        assert_eq!(value(&args, "-ub"), "40960");
    }
}
