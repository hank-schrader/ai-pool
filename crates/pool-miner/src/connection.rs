//! The miner's WebSocket session with the pool: registration, capacity,
//! heartbeats, token counts and jobs. Reconnects forever until shutdown.

mod counts;
mod execute;
mod session;
use execute::execute;

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use futures_util::{SinkExt, StreamExt};
use pool_protocol::{
    Backend, MinerMessage, Operation, PoolMessage,
    messages::{DeviceSlots, Hardware, JobErrorCode, MAX_MESSAGE_BYTES, PROTOCOL_VERSION, RejectReason, SUBPROTOCOL},
};
use pool_runtime::{RuntimeError, client::StreamItem};
use serde_json::Value;
use tokio::{
    sync::{Semaphore, mpsc, watch},
    task::JoinHandle,
};
use tokio_tungstenite::tungstenite::{
    Message,
    client::IntoClientRequest,
    http::{HeaderValue, header},
    protocol::WebSocketConfig,
};
use tokio_util::sync::CancellationToken;

use crate::{
    hosted::{Hosted, Ready},
    pool::Pool,
};

/// Concurrent token counts; counting needs no device slot.
const COUNT_CONCURRENCY: usize = 4;

pub struct Miner {
    pub pool: Pool,
    pub miner_id: String,
    pub catalog_revision: String,
    pub hardware: Hardware,
    pub device: String,
    pub models: Vec<Arc<Hosted>>,
    pub changes: watch::Receiver<u64>,
    /// H2O jobs accepted beyond the llama-server slots (`--prefetch`); unset
    /// means as many as the profile has slots.
    prefetch: OnceLock<u32>,
    /// Set by Ctrl-C: reject new jobs, finish active ones.
    pub draining: AtomicBool,
    /// Cancelled once draining is over; ends the session loop.
    pub shutdown: CancellationToken,
    /// Jobs accepted and not finished; limited by [`Miner::device_slots`].
    running: Arc<AtomicUsize>,
    counts: Arc<Semaphore>,
    active: Mutex<HashMap<String, JoinHandle<()>>>,
    active_counts: Mutex<HashMap<String, JoinHandle<()>>>,
    out: Mutex<Option<mpsc::Sender<MinerMessage>>>,
}

enum End {
    Shutdown,
    Lost(String),
}

impl Miner {
    pub fn new(
        pool: Pool,
        miner_id: String,
        catalog_revision: String,
        hardware: Hardware,
        device: String,
        models: Vec<Arc<Hosted>>,
        changes: watch::Receiver<u64>,
    ) -> Arc<Self> {
        Arc::new(Self {
            pool,
            miner_id,
            catalog_revision,
            hardware,
            device,
            models,
            changes,
            prefetch: OnceLock::new(),
            draining: AtomicBool::new(false),
            shutdown: CancellationToken::new(),
            running: Arc::new(AtomicUsize::new(0)),
            counts: Arc::new(Semaphore::new(COUNT_CONCURRENCY)),
            active: Mutex::new(HashMap::new()),
            active_counts: Mutex::new(HashMap::new()),
            out: Mutex::new(None),
        })
    }

    /// Sets the H2O prefetch; call before the session starts.
    pub fn set_prefetch(&self, prefetch: u32) {
        let _ = self.prefetch.set(prefetch);
    }

    pub fn active_jobs(&self) -> usize {
        self.active.lock().unwrap().len()
    }

    /// Ctrl-C: tell the pool, then wait for active work up to `grace`.
    pub async fn drain(&self, grace: Duration) {
        self.draining.store(true, Ordering::SeqCst);
        let out = self.out.lock().unwrap().clone();
        if let Some(out) = out {
            let _ = out.send(MinerMessage::Draining).await;
        }
        let deadline = Instant::now() + grace;
        while self.active_jobs() > 0 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        let left = self.active_jobs();
        if left > 0 {
            tracing::warn!("cancelling {left} job(s) still running after {}s", grace.as_secs());
        }
        self.shutdown.cancel();
    }

    fn capacity(&self) -> MinerMessage {
        MinerMessage::Capacity {
            devices: vec![DeviceSlots { id: self.device.clone(), slots: self.device_slots() }],
            models: self.models.iter().filter_map(|hosted| hosted.loaded()).collect(),
        }
    }

    /// Concurrent jobs the device accepts: per loaded model, the profile's
    /// llama-server slots, plus for H2O a prefetch of further jobs. A prefetched
    /// job is rendered, tokenized and sent while the slots are busy; llama-server
    /// queues its request until a slot frees, so the GPU is not left waiting for
    /// the next question to be prepared. With several models loaded it is the
    /// smallest of theirs, so a one-slot model (Clef, chat) never gets more than
    /// one job, and sharing a GPU keeps the one-job-at-a-time behaviour those
    /// models were qualified with.
    fn device_slots(&self) -> u32 {
        self.models
            .iter()
            .filter_map(|hosted| {
                let slots = hosted.ready()?.profile.slots();
                Some(job_slots(hosted.model.backend, slots, self.prefetch.get().copied()))
            })
            .min()
            .unwrap_or(1)
    }

    fn hosted(&self, model: &str) -> Option<&Arc<Hosted>> {
        self.models.iter().find(|hosted| hosted.model.id == model)
    }

    async fn handle(self: &Arc<Self>, message: PoolMessage, out: &mpsc::Sender<MinerMessage>) {
        match message {
            PoolMessage::Welcome { .. } | PoolMessage::Error { .. } => {}
            PoolMessage::CountTokens {
                count_id,
                model,
                model_revision,
                runtime_fingerprint,
                timeout_ms,
                operation: _,
                payload,
            } => {
                self.start_count(count_id, model, model_revision, runtime_fingerprint, timeout_ms, payload, out);
            }
            PoolMessage::CancelCount { count_id } => {
                if let Some(handle) = self.active_counts.lock().unwrap().remove(&count_id) {
                    handle.abort();
                }
            }
            PoolMessage::Job {
                job_id: _,
                attempt_id,
                model,
                model_revision,
                profile,
                runtime_fingerprint,
                operation,
                stream,
                input_tokens: _,
                required_context_tokens,
                timeout_ms,
                payload,
            } => {
                let checked = self.check_job(
                    &model,
                    &model_revision,
                    &profile,
                    required_context_tokens,
                    runtime_fingerprint.as_deref(),
                );
                let (ready, permit) = match checked {
                    Ok(ready) => ready,
                    Err((reason, message)) => {
                        let _ = out.send(MinerMessage::Rejected { attempt_id, reason, message }).await;
                        return;
                    }
                };
                // Accepted is queued before any result of this attempt
                let _ = out.send(MinerMessage::Accepted { attempt_id: attempt_id.clone() }).await;
                let miner = self.clone();
                let out = out.clone();
                let mut active = self.active.lock().unwrap();
                let id = attempt_id.clone();
                let handle = tokio::spawn(async move {
                    let _permit = permit;
                    // the deadline also covers the wait in llama-server's queue
                    let work = execute(&ready, operation, stream, &payload, &id, &out);
                    let outcome = tokio::time::timeout(Duration::from_millis(timeout_ms), work).await;
                    let error = match outcome {
                        Err(_) => {
                            Some(RuntimeError::new(JobErrorCode::Timeout, format!("no result within {timeout_ms} ms")))
                        }
                        Ok(Err(error)) => Some(error),
                        Ok(Ok(())) => None,
                    };
                    if let Some(error) = error {
                        if matches!(error.code, JobErrorCode::RuntimeFailed | JobErrorCode::OutOfMemory)
                            && let Some(hosted) = miner.hosted(&model)
                        {
                            hosted.report_broken(&ready.client);
                        }
                        let _ = out
                            .send(MinerMessage::JobError {
                                attempt_id: id.clone(),
                                code: error.code,
                                message: error.message,
                            })
                            .await;
                    }
                    miner.active.lock().unwrap().remove(&id);
                });
                active.insert(attempt_id, handle);
            }
            PoolMessage::Cancel { attempt_id } => {
                let handle = self.active.lock().unwrap().remove(&attempt_id);
                let out = out.clone();
                tokio::spawn(async move {
                    if let Some(handle) = handle {
                        // Dropping the HTTP future closes its runtime connection.
                        // llama-server applies cancellation between decode batches.
                        handle.abort();
                        let _ = handle.await;
                    }
                    let _ = out.send(MinerMessage::Cancelled { attempt_id }).await;
                });
            }
        }
    }

    async fn abort_all(&self) {
        let mut handles: Vec<_> = self.active.lock().unwrap().drain().map(|(_, handle)| handle).collect();
        handles.extend(self.active_counts.lock().unwrap().drain().map(|(_, handle)| handle));
        for handle in handles {
            handle.abort();
            let _ = handle.await;
        }
    }
}

pub fn job_slots(backend: Backend, slots: u32, prefetch: Option<u32>) -> u32 {
    match backend {
        Backend::LlamaServerH2o => slots.saturating_add(prefetch.unwrap_or(slots)),
        _ => 1,
    }
}

/// Frees a device slot when the job finishes, fails or is aborted.
struct SlotGuard(Arc<AtomicUsize>);

impl Drop for SlotGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

fn text(message: &MinerMessage) -> Message {
    Message::Text(serde_json::to_string(message).expect("message serializes").into())
}

fn truncate(text: &str, max: usize) -> &str {
    match text.char_indices().nth(max) {
        Some((index, _)) => &text[..index],
        None => text,
    }
}
