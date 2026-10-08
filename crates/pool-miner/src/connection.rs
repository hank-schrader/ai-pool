//! The miner's WebSocket session with the pool: registration, capacity,
//! heartbeats, token counts and jobs. Reconnects forever until shutdown.

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use futures_util::{SinkExt, StreamExt};
use pool_protocol::{
    MinerMessage, Operation, PoolMessage,
    messages::{DeviceSlots, Hardware, JobErrorCode, MAX_MESSAGE_BYTES, PROTOCOL_VERSION, RejectReason, SUBPROTOCOL},
};
use pool_runtime::{LocalClient, RuntimeError, client::StreamItem};
use serde_json::Value;
use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore, mpsc, watch},
    task::JoinHandle,
};
use tokio_tungstenite::tungstenite::{
    Message,
    client::IntoClientRequest,
    http::{HeaderValue, header},
    protocol::WebSocketConfig,
};
use tokio_util::sync::CancellationToken;

use crate::{hosted::Hosted, pool::Pool};

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
    /// Set by Ctrl-C: reject new jobs, finish active ones.
    pub draining: AtomicBool,
    /// Cancelled once draining is over; ends the session loop.
    pub shutdown: CancellationToken,
    slot: Arc<Semaphore>,
    counts: Arc<Semaphore>,
    active: Mutex<HashMap<String, JoinHandle<()>>>,
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
            draining: AtomicBool::new(false),
            shutdown: CancellationToken::new(),
            slot: Arc::new(Semaphore::new(1)),
            counts: Arc::new(Semaphore::new(COUNT_CONCURRENCY)),
            active: Mutex::new(HashMap::new()),
            out: Mutex::new(None),
        })
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

    /// Connects, serves, and reconnects with backoff until shutdown.
    pub async fn run(self: Arc<Self>) {
        let mut backoff = Duration::from_secs(1);
        loop {
            let started = Instant::now();
            let end = self.clone().session().await;
            *self.out.lock().unwrap() = None;
            self.abort_all().await;
            match end {
                Ok(End::Shutdown) => return,
                Ok(End::Lost(reason)) | Err(reason) => {
                    if self.shutdown.is_cancelled() {
                        return;
                    }
                    if started.elapsed() > Duration::from_secs(60) {
                        backoff = Duration::from_secs(1);
                    }
                    let jitter = Duration::from_millis((uuid::Uuid::new_v4().as_u128() % 1000) as u64);
                    tracing::warn!(
                        "pool connection: {reason}; reconnecting in {:.1}s",
                        (backoff + jitter).as_secs_f32()
                    );
                    tokio::select! {
                        _ = tokio::time::sleep(backoff + jitter) => {}
                        _ = self.shutdown.cancelled() => return,
                    }
                    backoff = (backoff * 2).min(Duration::from_secs(30));
                }
            }
        }
    }

    async fn session(self: Arc<Self>) -> Result<End, String> {
        let mut request = self.pool.connect_url().into_client_request().map_err(|error| error.to_string())?;
        request.headers_mut().insert(header::SEC_WEBSOCKET_PROTOCOL, HeaderValue::from_static(SUBPROTOCOL));
        if let Some(token) = self.pool.token_value() {
            let value = HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| "invalid miner token")?;
            request.headers_mut().insert(header::AUTHORIZATION, value);
        }
        let config = WebSocketConfig::default()
            .max_message_size(Some(MAX_MESSAGE_BYTES))
            .max_frame_size(Some(MAX_MESSAGE_BYTES));
        let connect = tokio_tungstenite::connect_async_with_config(request, Some(config), true);
        let (socket, _) = tokio::time::timeout(Duration::from_secs(15), connect)
            .await
            .map_err(|_| "connect timed out".to_string())?
            .map_err(|error| format!("cannot connect to {}: {error}", self.pool.connect_url()))?;
        let (mut sink, mut stream) = socket.split();

        let hello = MinerMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            miner_id: self.miner_id.clone(),
            miner_version: env!("CARGO_PKG_VERSION").into(),
            catalog_revision: self.catalog_revision.clone(),
            hardware: self.hardware.clone(),
        };
        sink.send(text(&hello)).await.map_err(|error| error.to_string())?;
        let heartbeat_seconds = loop {
            let frame = tokio::time::timeout(Duration::from_secs(15), stream.next())
                .await
                .map_err(|_| "no welcome from the pool".to_string())?
                .ok_or("connection closed before welcome")?
                .map_err(|error| error.to_string())?;
            match frame {
                Message::Text(body) => match serde_json::from_str::<PoolMessage>(&body) {
                    Ok(PoolMessage::Welcome { session_id, heartbeat_seconds }) => {
                        tracing::info!("connected to {} (session {session_id})", self.pool.url());
                        break heartbeat_seconds.max(1);
                    }
                    Ok(PoolMessage::Error { code, message }) => {
                        return Err(format!("pool refused the miner: {code}: {message}"));
                    }
                    _ => return Err(format!("unexpected first message: {body}")),
                },
                Message::Close(frame) => return Err(format!("pool closed the connection: {frame:?}")),
                _ => {}
            }
        };

        let (out, mut outbox) = mpsc::channel::<MinerMessage>(256);
        *self.out.lock().unwrap() = Some(out.clone());
        let writer = tokio::spawn(async move {
            while let Some(message) = outbox.recv().await {
                if sink.send(text(&message)).await.is_err() {
                    break;
                }
            }
            let _ = sink.close().await;
        });

        if self.draining.load(Ordering::SeqCst) {
            let _ = out.send(MinerMessage::Draining).await;
        }
        let _ = out.send(self.capacity()).await;
        let mut changes = self.changes.clone();
        changes.mark_unchanged();
        let mut heartbeat = tokio::time::interval(Duration::from_secs(heartbeat_seconds));
        heartbeat.tick().await;

        let end = loop {
            tokio::select! {
                _ = self.shutdown.cancelled() => break Ok(End::Shutdown),
                _ = heartbeat.tick() => {
                    let active_attempts = self.active.lock().unwrap().keys().cloned().collect();
                    if out.send(MinerMessage::Heartbeat { active_attempts }).await.is_err() {
                        break Ok(End::Lost("writer stopped".into()));
                    }
                }
                changed = changes.changed() => {
                    if changed.is_ok() {
                        let _ = out.send(self.capacity()).await;
                    }
                }
                frame = stream.next() => match frame {
                    None => break Ok(End::Lost("connection closed".into())),
                    Some(Err(error)) => break Ok(End::Lost(error.to_string())),
                    Some(Ok(Message::Text(body))) => match serde_json::from_str::<PoolMessage>(&body) {
                        Ok(PoolMessage::Error { code, message }) => break Ok(End::Lost(format!("pool error {code}: {message}"))),
                        Ok(message) => self.handle(message, &out).await,
                        Err(error) => tracing::warn!("ignoring unknown pool message ({error}): {}", truncate(&body, 200)),
                    },
                    Some(Ok(Message::Close(frame))) => break Ok(End::Lost(format!("pool closed the connection: {frame:?}"))),
                    Some(Ok(_)) => {}
                },
            }
        };
        drop(out);
        writer.abort();
        end
    }

    fn capacity(&self) -> MinerMessage {
        MinerMessage::Capacity {
            devices: vec![DeviceSlots { id: self.device.clone(), slots: 1 }],
            models: self.models.iter().filter_map(|hosted| hosted.loaded()).collect(),
        }
    }

    fn hosted(&self, model: &str) -> Option<&Arc<Hosted>> {
        self.models.iter().find(|hosted| hosted.model.id == model)
    }

    async fn handle(self: &Arc<Self>, message: PoolMessage, out: &mpsc::Sender<MinerMessage>) {
        match message {
            PoolMessage::Welcome { .. } | PoolMessage::Error { .. } => {}
            PoolMessage::CountTokens { count_id, model, model_revision, operation: _, payload } => {
                let miner = self.clone();
                let out = out.clone();
                tokio::spawn(async move {
                    let reply = match miner.hosted(&model) {
                        Some(hosted) if hosted.revision == model_revision => {
                            let _permit = miner.counts.acquire().await;
                            match hosted.count(&payload).await {
                                Ok(input_tokens) => MinerMessage::CountResult { count_id, input_tokens },
                                Err(error) => {
                                    MinerMessage::CountError { count_id, code: error.code, message: error.message }
                                }
                            }
                        }
                        _ => MinerMessage::CountError {
                            count_id,
                            code: JobErrorCode::RuntimeFailed,
                            message: format!("{model} at revision {model_revision} is not served here"),
                        },
                    };
                    let _ = out.send(reply).await;
                });
            }
            PoolMessage::Job {
                job_id: _,
                attempt_id,
                model,
                model_revision,
                profile,
                operation,
                stream,
                input_tokens: _,
                required_context_tokens,
                timeout_ms,
                payload,
            } => {
                let checked = self.check_job(&model, &model_revision, &profile, required_context_tokens);
                let (client, permit) = match checked {
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
                    let work = execute(&client, operation, stream, &payload, &id, &out);
                    let outcome = tokio::time::timeout(Duration::from_millis(timeout_ms), work).await;
                    let error = match outcome {
                        Err(_) => {
                            Some(RuntimeError::new(JobErrorCode::Timeout, format!("no result within {timeout_ms} ms")))
                        }
                        Ok(Err(error)) => Some(error),
                        Ok(Ok(())) => None,
                    };
                    if let Some(error) = error {
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
                        // dropping the request future disconnects from the runtime, which stops the work
                        handle.abort();
                        let _ = handle.await;
                    }
                    let _ = out.send(MinerMessage::Cancelled { attempt_id }).await;
                });
            }
        }
    }

    /// Whether this miner can run the job now; takes the device slot if so.
    fn check_job(
        &self,
        model: &str,
        revision: &str,
        profile: &str,
        required_context: u32,
    ) -> Result<(LocalClient, OwnedSemaphorePermit), (RejectReason, String)> {
        if self.draining.load(Ordering::SeqCst) {
            return Err((RejectReason::Draining, "miner is shutting down".into()));
        }
        let Some(hosted) = self.hosted(model) else {
            return Err((RejectReason::ModelUnavailable, format!("{model} is not served here")));
        };
        if hosted.revision != revision {
            return Err((RejectReason::RevisionMismatch, format!("serving {}", hosted.revision)));
        }
        if hosted.profile.id != profile {
            return Err((RejectReason::ModelUnavailable, format!("serving profile {}", hosted.profile.id)));
        }
        if hosted.profile.context_tokens < required_context {
            return Err((
                RejectReason::ContextTooSmall,
                format!("context {} < required {required_context}", hosted.profile.context_tokens),
            ));
        }
        let Some(client) = hosted.client() else {
            return Err((RejectReason::ModelUnavailable, "runtime is restarting".into()));
        };
        let Ok(permit) = self.slot.clone().try_acquire_owned() else {
            return Err((RejectReason::Busy, "device slot is busy".into()));
        };
        Ok((client, permit))
    }

    async fn abort_all(&self) {
        let handles: Vec<_> = self.active.lock().unwrap().drain().map(|(_, handle)| handle).collect();
        for handle in handles {
            handle.abort();
            let _ = handle.await;
        }
    }
}

async fn execute(
    client: &LocalClient,
    operation: Operation,
    stream: bool,
    payload: &Value,
    attempt_id: &str,
    out: &mpsc::Sender<MinerMessage>,
) -> Result<(), RuntimeError> {
    let result = |body| MinerMessage::Result { attempt_id: attempt_id.to_string(), body };
    match (operation, stream) {
        (Operation::Systemone, _) => {
            let body = client.systemone(payload).await?;
            let _ = out.send(result(body)).await;
        }
        (Operation::ChatCompletions, false) => {
            let body = client.chat(payload).await?;
            let _ = out.send(result(body)).await;
        }
        (Operation::ChatCompletions, true) => {
            let mut events = client.chat_stream(payload).await?;
            let _ = out.send(MinerMessage::StreamStart { attempt_id: attempt_id.into() }).await;
            let mut seq = 0;
            let mut usage = None;
            loop {
                match events.next().await {
                    None => return Err(RuntimeError::new(JobErrorCode::RuntimeFailed, "stream ended without [DONE]")),
                    Some(Err(error)) => return Err(error),
                    Some(Ok(StreamItem::Done)) => break,
                    Some(Ok(StreamItem::Chunk(chunk))) => {
                        let usage_only = chunk.get("choices").and_then(Value::as_array).is_some_and(Vec::is_empty)
                            && chunk.get("usage").is_some();
                        if usage_only {
                            usage = chunk.get("usage").cloned();
                            continue;
                        }
                        // waiting here applies the pool's backpressure to the runtime
                        if out
                            .send(MinerMessage::StreamChunk { attempt_id: attempt_id.into(), seq, chunk })
                            .await
                            .is_err()
                        {
                            return Ok(());
                        }
                        seq += 1;
                    }
                }
            }
            let _ = out.send(MinerMessage::StreamEnd { attempt_id: attempt_id.into(), usage }).await;
        }
    }
    Ok(())
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
