//! The `clef-token-count` helper: exact Clef input counts from the pinned
//! llama.cpp prompt code, without a GPU (see `native/clef-token-count`).

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use pool_protocol::messages::JobErrorCode;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::Mutex,
};

use crate::{Error, Result, RuntimeError};

const BINARY: &str = if cfg!(windows) { "clef-token-count.exe" } else { "clef-token-count" };
const REPLY_TIMEOUT: Duration = Duration::from_secs(10);

/// Finds the helper: explicit path (flag or `AI_POOL_CLEF_HELPER`), next to the
/// miner executable, then the source-tree build directory.
pub fn locate(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return if path.is_file() {
            Ok(path.to_path_buf())
        } else {
            Err(Error::msg(format!("Clef token-count helper not found at {}", path.display())))
        };
    }
    let mut candidates = Vec::new();
    // resolve symlinks: installers link the miner into ~/.local/bin
    if let Ok(exe) = std::env::current_exe().and_then(std::fs::canonicalize)
        && let Some(dir) = exe.parent()
    {
        candidates.push(dir.join(BINARY));
    }
    candidates.push(PathBuf::from("build").join("clef-token-count").join(BINARY));
    candidates.into_iter().find(|path| path.is_file()).ok_or_else(|| {
        Error::msg(
            "the Clef token-count helper was not found. Build it with `scripts/build-helper.sh` \
             (or pass --clef-helper / set AI_POOL_CLEF_HELPER)",
        )
    })
}

struct Process {
    _child: Child,
    stdin: ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
}

pub struct ClefCounter {
    path: PathBuf,
    model: PathBuf,
    commit: String,
    process: Mutex<Option<Process>>,
    next_id: AtomicU64,
}

impl ClefCounter {
    /// Starts the helper for `model` and checks it was built from `commit`.
    pub async fn start(path: PathBuf, model: PathBuf, commit: &str) -> Result<Self> {
        let counter =
            Self { path, model, commit: commit.into(), process: Mutex::new(None), next_id: AtomicU64::new(0) };
        let process = counter.spawn().await?;
        *counter.process.lock().await = Some(process);
        Ok(counter)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    async fn spawn(&self) -> Result<Process> {
        let mut child = Command::new(&self.path)
            .arg(&self.model)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| Error::msg(format!("cannot start {}: {error}", self.path.display())))?;
        let stdin = child.stdin.take().expect("piped stdin");
        let mut stdout = BufReader::new(child.stdout.take().expect("piped stdout")).lines();
        let ready = tokio::time::timeout(Duration::from_secs(60), stdout.next_line())
            .await
            .map_err(|_| Error::msg("Clef token-count helper did not become ready"))??
            .ok_or_else(|| Error::msg("Clef token-count helper exited during startup"))?;
        let ready: Value = serde_json::from_str(&ready)?;
        if ready["ready"] != json!(true) {
            return Err(Error::msg(format!("Clef token-count helper failed: {ready}")));
        }
        let built = ready["llama_cpp_commit"].as_str().unwrap_or_default();
        if built != self.commit {
            return Err(Error::msg(format!(
                "Clef token-count helper was built from llama.cpp {built}, expected {}; rebuild it",
                self.commit
            )));
        }
        Ok(Process { _child: child, stdin, stdout })
    }

    /// Exact input tokens of a systemone payload (`{"state", "questions"}`).
    pub async fn count(&self, payload: &Value) -> std::result::Result<u32, RuntimeError> {
        let mut guard = self.process.lock().await;
        for attempt in 0..2 {
            if guard.is_none() {
                match self.spawn().await {
                    Ok(process) => *guard = Some(process),
                    Err(error) => return Err(RuntimeError::new(JobErrorCode::RuntimeFailed, error.to_string())),
                }
            }
            let process = guard.as_mut().expect("helper running");
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            match exchange(process, id, payload).await {
                Ok(reply) => return parse_reply(id, &reply),
                Err(error) => {
                    // a broken helper is restarted once for this request
                    tracing::warn!("Clef token-count helper failed ({error}); restarting");
                    *guard = None;
                    if attempt == 1 {
                        return Err(RuntimeError::new(JobErrorCode::RuntimeFailed, error));
                    }
                }
            }
        }
        unreachable!()
    }
}

async fn exchange(process: &mut Process, id: u64, payload: &Value) -> std::result::Result<Value, String> {
    let mut line = serde_json::to_string(&json!({ "id": id, "body": payload })).map_err(|e| e.to_string())?;
    line.push('\n');
    process.stdin.write_all(line.as_bytes()).await.map_err(|e| e.to_string())?;
    process.stdin.flush().await.map_err(|e| e.to_string())?;
    let reply = tokio::time::timeout(REPLY_TIMEOUT, process.stdout.next_line())
        .await
        .map_err(|_| "no reply within 10 s".to_string())?
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "helper exited".to_string())?;
    serde_json::from_str(&reply).map_err(|e| format!("invalid helper reply: {e}"))
}

fn parse_reply(id: u64, reply: &Value) -> std::result::Result<u32, RuntimeError> {
    if reply["id"] != json!(id) {
        return Err(RuntimeError::new(JobErrorCode::Internal, "helper reply id mismatch"));
    }
    if let Some(tokens) = reply["input_tokens"].as_u64() {
        return Ok(tokens as u32);
    }
    let message = reply.pointer("/error/message").and_then(Value::as_str).unwrap_or("unknown helper error");
    let code = match reply.pointer("/error/kind").and_then(Value::as_str) {
        Some("invalid_request") => JobErrorCode::InvalidRequest,
        _ => JobErrorCode::Internal,
    };
    Err(RuntimeError::new(code, message))
}
