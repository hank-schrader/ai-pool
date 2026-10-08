//! The scheduler: a single task that owns miners, device slots, the job queue
//! and pending token counts. HTTP handlers and miner connections talk to it
//! through commands, so slot reservation, cancellation and retries never race.
//!
//! Guarantees: a client gets at most one response. An attempt is retried once,
//! on another miner, only before anything was committed to the client. A slot
//! stays reserved until the miner confirms the work stopped or disconnects.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::http::StatusCode;
use pool_protocol::{
    Catalog,
    messages::{DeviceSlots, Hardware, JobErrorCode, LoadedModel, MinerMessage, Operation, PoolMessage, RejectReason},
    requests::Validated,
};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::{
    sync::{mpsc, oneshot},
    time::{Instant, MissedTickBehavior},
};
use tracing::{debug, info, warn};

use crate::{config::Config, error::ApiError};

/// Infrastructure retries after the first attempt.
const MAX_RETRIES: u32 = 1;
/// Rejections (e.g. a miner that was busy after all) before giving up.
const MAX_REJECTIONS: u32 = 3;
const MAX_PENDING_COUNTS: usize = 256;

pub enum JobEvent {
    /// Complete non-streaming response.
    Result {
        body: Value,
        profile: String,
    },
    /// One streamed chunk; the first one commits the response.
    Chunk(Value),
    /// Successful end of a stream.
    End,
    Failed(ApiError),
}

pub struct NewJob {
    pub id: String,
    pub client: String,
    pub model: String,
    pub model_revision: String,
    pub request: Validated,
    pub input_tokens: u32,
    pub required_context: u32,
}

pub struct CountRequest {
    pub model: String,
    pub model_revision: String,
    pub operation: Operation,
    pub payload: Value,
}

enum Command {
    MinerConnected {
        session: String,
        label: String,
        miner_id: String,
        hardware: Hardware,
        outbound: mpsc::Sender<PoolMessage>,
    },
    MinerMessage {
        session: String,
        message: MinerMessage,
    },
    MinerGone {
        session: String,
    },
    Count {
        request: CountRequest,
        reply: oneshot::Sender<Result<u32, ApiError>>,
    },
    Submit {
        job: NewJob,
        events: mpsc::Sender<JobEvent>,
        reply: oneshot::Sender<Result<(), ApiError>>,
    },
    Cancel {
        job_id: String,
    },
    Snapshot {
        reply: oneshot::Sender<Snapshot>,
    },
}

#[derive(Clone)]
pub struct SchedulerHandle {
    commands: mpsc::UnboundedSender<Command>,
}

impl SchedulerHandle {
    fn send(&self, command: Command) {
        // The scheduler only stops when the process does.
        let _ = self.commands.send(command);
    }

    pub fn miner_connected(
        &self,
        session: &str,
        label: &str,
        miner_id: &str,
        hardware: Hardware,
        outbound: mpsc::Sender<PoolMessage>,
    ) {
        self.send(Command::MinerConnected {
            session: session.into(),
            label: label.into(),
            miner_id: miner_id.into(),
            hardware,
            outbound,
        });
    }

    pub fn miner_message(&self, session: &str, message: MinerMessage) {
        self.send(Command::MinerMessage { session: session.into(), message });
    }

    pub fn miner_gone(&self, session: &str) {
        self.send(Command::MinerGone { session: session.into() });
    }

    pub async fn count(&self, request: CountRequest) -> Result<u32, ApiError> {
        let (reply, response) = oneshot::channel();
        self.send(Command::Count { request, reply });
        response.await.unwrap_or_else(|_| Err(ApiError::miner_failed("scheduler stopped")))
    }

    pub async fn submit(&self, job: NewJob, buffer: usize) -> Result<mpsc::Receiver<JobEvent>, ApiError> {
        let (events, receiver) = mpsc::channel(buffer.max(1));
        let (reply, response) = oneshot::channel();
        self.send(Command::Submit { job, events, reply });
        response.await.unwrap_or_else(|_| Err(ApiError::miner_failed("scheduler stopped")))?;
        Ok(receiver)
    }

    pub fn cancel(&self, job_id: &str) {
        self.send(Command::Cancel { job_id: job_id.into() });
    }

    pub async fn snapshot(&self) -> Snapshot {
        let (reply, response) = oneshot::channel();
        self.send(Command::Snapshot { reply });
        response.await.unwrap_or_default()
    }
}

/// Cancels its job when dropped before [`JobGuard::finish`], e.g. when the
/// client disconnects and axum drops the handler or the response stream.
pub struct JobGuard {
    handle: SchedulerHandle,
    job_id: String,
    finished: bool,
}

impl JobGuard {
    pub fn new(handle: SchedulerHandle, job_id: &str) -> Self {
        Self { handle, job_id: job_id.into(), finished: false }
    }

    pub fn finish(&mut self) {
        self.finished = true;
    }
}

impl Drop for JobGuard {
    fn drop(&mut self) {
        if !self.finished {
            self.handle.cancel(&self.job_id);
        }
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Snapshot {
    pub models: HashMap<String, ModelAvailability>,
    pub miners: Vec<MinerStatus>,
    pub queued_jobs: usize,
    pub active_jobs: usize,
    pub pending_counts: usize,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ModelAvailability {
    pub ready_miners: usize,
    pub free_slots: usize,
    pub max_available_context_tokens: u32,
    pub max_idle_context_tokens: u32,
    pub queued_requests: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct MinerStatus {
    pub session_id: String,
    pub label: String,
    pub miner_id: String,
    pub hardware: Hardware,
    pub devices: Vec<DeviceStatus>,
    pub models: Vec<LoadedModel>,
    pub draining: bool,
    pub seconds_since_seen: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct DeviceStatus {
    pub id: String,
    pub slots: u32,
    pub busy: u32,
}

struct Miner {
    label: String,
    miner_id: String,
    hardware: Hardware,
    outbound: mpsc::Sender<PoolMessage>,
    devices: Vec<DeviceSlots>,
    models: Vec<LoadedModel>,
    draining: bool,
    last_seen: Instant,
    /// Attempts holding a device slot. `job` is None once the job is gone
    /// but the miner has not yet confirmed the work stopped.
    attempts: HashMap<String, Attempt>,
    pending_counts: usize,
}

struct Attempt {
    job: Option<String>,
    device: String,
}

impl Miner {
    fn busy(&self, device: &str) -> u32 {
        self.attempts.values().filter(|attempt| attempt.device == device).count() as u32
    }

    fn slots(&self, device: &str) -> u32 {
        self.devices.iter().find(|d| d.id == device).map_or(0, |d| d.slots)
    }

    fn serves(&self, model: &str, revision: &str) -> impl Iterator<Item = &LoadedModel> {
        self.models.iter().filter(move |m| m.model == model && m.model_revision == revision)
    }
}

enum JobState {
    Queued,
    Dispatched { session: String, attempt: String, profile: String, sent_at: Instant, accepted: bool },
}

struct Job {
    new: NewJob,
    events: mpsc::Sender<JobEvent>,
    created_unix: u64,
    deadline: Instant,
    queue_deadline: Instant,
    first_chunk_deadline: Instant,
    state: JobState,
    attempts: u32,
    rejections: u32,
    excluded: HashSet<String>,
    committed: bool,
    next_seq: u64,
}

struct PendingCount {
    request: CountRequest,
    reply: oneshot::Sender<Result<u32, ApiError>>,
    session: String,
    deadline: Instant,
}

pub struct Scheduler {
    config: Config,
    catalog: Arc<Catalog>,
    miners: HashMap<String, Miner>,
    jobs: HashMap<String, Job>,
    queue: VecDeque<String>,
    counts: HashMap<String, PendingCount>,
    rotation: usize,
}

pub fn spawn(config: Config, catalog: Arc<Catalog>) -> SchedulerHandle {
    let (commands, receiver) = mpsc::unbounded_channel();
    let scheduler = Scheduler {
        config,
        catalog,
        miners: HashMap::new(),
        jobs: HashMap::new(),
        queue: VecDeque::new(),
        counts: HashMap::new(),
        rotation: 0,
    };
    tokio::spawn(scheduler.run(receiver));
    SchedulerHandle { commands }
}

impl Scheduler {
    async fn run(mut self, mut commands: mpsc::UnboundedReceiver<Command>) {
        let mut tick = tokio::time::interval(Duration::from_millis(250));
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                command = commands.recv() => match command {
                    Some(command) => self.handle(command),
                    None => return,
                },
                _ = tick.tick() => self.expire(Instant::now()),
            }
        }
    }

    fn handle(&mut self, command: Command) {
        match command {
            Command::MinerConnected { session, label, miner_id, hardware, outbound } => {
                info!(%session, %label, %miner_id, accelerator = %hardware.accelerator, "miner connected");
                self.miners.insert(
                    session,
                    Miner {
                        label,
                        miner_id,
                        hardware,
                        outbound,
                        devices: Vec::new(),
                        models: Vec::new(),
                        draining: false,
                        last_seen: Instant::now(),
                        attempts: HashMap::new(),
                        pending_counts: 0,
                    },
                );
            }
            Command::MinerMessage { session, message } => self.on_miner_message(&session, message),
            Command::MinerGone { session } => self.remove_miner(&session, "miner disconnected"),
            Command::Count { request, reply } => self.start_count(request, reply),
            Command::Submit { job, events, reply } => {
                let result = self.admit(job, events);
                let _ = reply.send(result);
                self.pump();
            }
            Command::Cancel { job_id } => {
                if self.jobs.contains_key(&job_id) {
                    debug!(job = %job_id, "client went away; cancelling");
                    self.drop_job(&job_id, None);
                    self.pump();
                }
            }
            Command::Snapshot { reply } => {
                let _ = reply.send(self.snapshot());
            }
        }
    }

    // ---- admission and dispatch -------------------------------------------------

    fn admit(&mut self, new: NewJob, events: mpsc::Sender<JobEvent>) -> Result<(), ApiError> {
        let ready: Vec<u32> = self
            .miners
            .values()
            .filter(|miner| !miner.draining)
            .flat_map(|miner| miner.serves(&new.model, &new.model_revision).map(|m| m.context_tokens))
            .collect();
        if ready.is_empty() {
            return Err(ApiError::no_miner(&new.model));
        }
        let largest = ready.iter().copied().max().unwrap_or(0);
        if largest < new.required_context {
            return Err(ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "context_unavailable",
                format!(
                    "this request needs {} tokens of context; the largest loaded profile of {:?} has {largest}",
                    new.required_context, new.model
                ),
            )
            .retry_after(30));
        }
        let client_jobs = self.jobs.values().filter(|job| job.new.client == new.client).count();
        if client_jobs >= self.config.max_active_per_client + self.config.max_queued_per_client {
            return Err(ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limit_exceeded",
                format!("at most {} requests may run or wait per client", client_jobs),
            )
            .retry_after(1));
        }
        let model_queued = self.queue.iter().filter(|id| self.jobs[*id].new.model == new.model).count();
        if self.queue.len() >= self.config.queue_capacity_global || model_queued >= self.config.queue_capacity_per_model
        {
            return Err(ApiError::overloaded("the pool queue is full"));
        }

        let now = Instant::now();
        let id = new.id.clone();
        self.jobs.insert(
            id.clone(),
            Job {
                created_unix: SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()),
                deadline: now + self.config.request_timeout,
                queue_deadline: now + self.config.queue_timeout,
                first_chunk_deadline: now + self.config.first_chunk_timeout,
                new,
                events,
                state: JobState::Queued,
                attempts: 0,
                rejections: 0,
                excluded: HashSet::new(),
                committed: false,
                next_seq: 0,
            },
        );
        self.queue.push_back(id);
        Ok(())
    }

    /// Dispatches every queued job that has a fitting free slot, oldest first.
    /// A job that cannot be placed does not block smaller ones behind it.
    fn pump(&mut self) {
        let mut index = 0;
        while index < self.queue.len() {
            let job_id = self.queue[index].clone();
            let job = &self.jobs[&job_id];
            let client_active = self
                .jobs
                .values()
                .filter(|other| {
                    other.new.client == job.new.client && matches!(other.state, JobState::Dispatched { .. })
                })
                .count();
            if client_active >= self.config.max_active_per_client {
                index += 1;
                continue;
            }
            let slot = self.find_slot(job, self.rotation);
            self.rotation = self.rotation.wrapping_add(1);
            match slot {
                Some((session, device, profile)) => {
                    self.queue.remove(index);
                    self.dispatch(&job_id, &session, &device, &profile);
                }
                None => index += 1,
            }
        }
    }

    /// Smallest fitting context first, then the least busy device, then round robin.
    fn find_slot(&self, job: &Job, rotation: usize) -> Option<(String, String, String)> {
        let mut candidates: Vec<(u32, u32, String, String, String)> = Vec::new();
        for (session, miner) in &self.miners {
            if miner.draining || job.excluded.contains(session) {
                continue;
            }
            for loaded in miner.serves(&job.new.model, &job.new.model_revision) {
                let busy = miner.busy(&loaded.device);
                if loaded.context_tokens >= job.new.required_context && busy < miner.slots(&loaded.device) {
                    candidates.push((
                        loaded.context_tokens,
                        busy,
                        session.clone(),
                        loaded.device.clone(),
                        loaded.profile.clone(),
                    ));
                }
            }
        }
        candidates.sort();
        let best = candidates.first().map(|c| (c.0, c.1))?;
        let tied: Vec<_> = candidates.into_iter().filter(|c| (c.0, c.1) == best).collect();
        let (_, _, session, device, profile) = tied[rotation % tied.len()].clone();
        Some((session, device, profile))
    }

    fn dispatch(&mut self, job_id: &str, session: &str, device: &str, profile: &str) {
        let attempt = format!("att_{}", uuid::Uuid::new_v4().simple());
        let job = self.jobs.get_mut(job_id).expect("queued job exists");
        let now = Instant::now();
        let message = PoolMessage::Job {
            job_id: job_id.into(),
            attempt_id: attempt.clone(),
            model: job.new.model.clone(),
            model_revision: job.new.model_revision.clone(),
            profile: profile.into(),
            operation: job.new.request.operation,
            stream: job.new.request.stream,
            input_tokens: job.new.input_tokens,
            required_context_tokens: job.new.required_context,
            timeout_ms: job.deadline.saturating_duration_since(now).as_millis() as u64,
            payload: job.new.request.payload.clone(),
        };
        let miner = self.miners.get_mut(session).expect("candidate miner exists");
        if miner.outbound.try_send(message).is_err() {
            warn!(%session, "miner outbound queue is full or closed; disconnecting it");
            self.queue.push_front(job_id.into());
            self.remove_miner(session, "miner stopped reading");
            return;
        }
        miner.attempts.insert(attempt.clone(), Attempt { job: Some(job_id.into()), device: device.into() });
        debug!(job = %job_id, %attempt, %session, %profile, "dispatched");
        job.attempts += 1;
        job.state = JobState::Dispatched {
            session: session.into(),
            attempt,
            profile: profile.into(),
            sent_at: now,
            accepted: false,
        };
    }

    // ---- token counting -----------------------------------------------------------

    fn start_count(&mut self, request: CountRequest, reply: oneshot::Sender<Result<u32, ApiError>>) {
        if self.counts.len() >= MAX_PENDING_COUNTS {
            let _ = reply.send(Err(ApiError::overloaded("too many requests are being counted")));
            return;
        }
        let deadline = Instant::now() + self.config.count_timeout;
        self.send_count(request, reply, deadline, &HashSet::new());
    }

    fn send_count(
        &mut self,
        request: CountRequest,
        reply: oneshot::Sender<Result<u32, ApiError>>,
        deadline: Instant,
        excluded: &HashSet<String>,
    ) {
        let session = self
            .miners
            .iter()
            .filter(|(session, miner)| {
                !miner.draining
                    && !excluded.contains(*session)
                    && miner.serves(&request.model, &request.model_revision).next().is_some()
            })
            .min_by_key(|(session, miner)| (miner.pending_counts, (*session).clone()))
            .map(|(session, _)| session.clone());
        let Some(session) = session else {
            let _ = reply.send(Err(ApiError::no_miner(&request.model)));
            return;
        };
        let count_id = format!("cnt_{}", uuid::Uuid::new_v4().simple());
        let message = PoolMessage::CountTokens {
            count_id: count_id.clone(),
            model: request.model.clone(),
            model_revision: request.model_revision.clone(),
            operation: request.operation,
            payload: request.payload.clone(),
        };
        let miner = self.miners.get_mut(&session).expect("selected miner exists");
        if miner.outbound.try_send(message).is_err() {
            let _ = reply.send(Err(ApiError::overloaded("miner connection is congested")));
            return;
        }
        miner.pending_counts += 1;
        self.counts.insert(count_id, PendingCount { request, reply, session, deadline });
    }

    fn finish_count(&mut self, count_id: &str, result: Result<u32, ApiError>) {
        if let Some(pending) = self.counts.remove(count_id) {
            if let Some(miner) = self.miners.get_mut(&pending.session) {
                miner.pending_counts = miner.pending_counts.saturating_sub(1);
            }
            let _ = pending.reply.send(result);
        }
    }

    // ---- miner messages ---------------------------------------------------------------

    fn on_miner_message(&mut self, session: &str, message: MinerMessage) {
        let Some(miner) = self.miners.get_mut(session) else { return };
        miner.last_seen = Instant::now();
        match message {
            MinerMessage::Capacity { devices, models } => self.on_capacity(session, devices, models),
            MinerMessage::Heartbeat { .. } | MinerMessage::Hello { .. } => {}
            MinerMessage::Draining => {
                info!(%session, "miner is draining");
                if let Some(miner) = self.miners.get_mut(session) {
                    miner.draining = true;
                }
            }
            MinerMessage::CountResult { count_id, input_tokens } => self.finish_count(&count_id, Ok(input_tokens)),
            MinerMessage::CountError { count_id, code, message } => {
                let error = match code {
                    JobErrorCode::InvalidRequest => ApiError::bad_request("invalid_request", message),
                    JobErrorCode::ContextLengthExceeded => ApiError::bad_request("context_length_exceeded", message),
                    _ => ApiError::miner_failed(format!("token counting failed: {message}")),
                };
                self.finish_count(&count_id, Err(error));
            }
            MinerMessage::Accepted { attempt_id } => {
                if let Some(job) = self.job_for(session, &attempt_id)
                    && let JobState::Dispatched { accepted, .. } = &mut job.state
                {
                    *accepted = true;
                }
            }
            MinerMessage::Rejected { attempt_id, reason, message } => {
                debug!(%session, attempt = %attempt_id, ?reason, %message, "miner rejected job");
                if let Some(job_id) = self.release(session, &attempt_id) {
                    self.requeue_rejected(&job_id, session, reason);
                }
            }
            MinerMessage::Result { attempt_id, body } => {
                if let Some(job_id) = self.release(session, &attempt_id) {
                    let job = self.jobs.remove(&job_id).expect("released job exists");
                    let profile = match &job.state {
                        JobState::Dispatched { profile, .. } => profile.clone(),
                        JobState::Queued => String::new(),
                    };
                    let body = shape_response(&job, body);
                    let _ = job.events.try_send(JobEvent::Result { body, profile });
                }
            }
            MinerMessage::StreamStart { .. } => {}
            MinerMessage::StreamChunk { attempt_id, seq, chunk } => self.on_chunk(session, &attempt_id, seq, chunk),
            MinerMessage::StreamEnd { attempt_id, usage } => {
                if let Some(job_id) = self.release(session, &attempt_id) {
                    let job = self.jobs.remove(&job_id).expect("released job exists");
                    if job.new.request.include_usage
                        && let Some(usage) = usage
                    {
                        let chunk = shape_chunk(
                            &job,
                            json!({"object": "chat.completion.chunk", "choices": [], "usage": usage}),
                        );
                        let _ = job.events.try_send(JobEvent::Chunk(chunk));
                    }
                    let _ = job.events.try_send(JobEvent::End);
                }
            }
            MinerMessage::JobError { attempt_id, code, message } => {
                warn!(%session, attempt = %attempt_id, ?code, %message, "job failed on miner");
                if let Some(job_id) = self.release(session, &attempt_id) {
                    self.on_job_error(&job_id, session, code, message);
                }
            }
            MinerMessage::Cancelled { attempt_id } => {
                self.release(session, &attempt_id);
            }
        }
        self.pump();
    }

    fn on_capacity(&mut self, session: &str, devices: Vec<DeviceSlots>, models: Vec<LoadedModel>) {
        let mut accepted = Vec::new();
        for loaded in models {
            let valid = self.catalog.model(&loaded.model).is_some_and(|model| {
                self.catalog.model_revision(model) == loaded.model_revision
                    && model.profile(&loaded.profile).is_some_and(|p| p.context_tokens == loaded.context_tokens)
            }) && devices.iter().any(|device| device.id == loaded.device);
            if valid {
                accepted.push(loaded);
            } else {
                warn!(%session, model = %loaded.model, profile = %loaded.profile, "ignoring a loaded model that does not match the catalog");
            }
        }
        let miner = self.miners.get_mut(session).expect("checked by caller");
        info!(
            %session,
            label = %miner.label,
            models = ?accepted.iter().map(|m| format!("{}@{}", m.model, m.profile)).collect::<Vec<_>>(),
            "miner capacity"
        );
        miner.devices = devices;
        miner.models = accepted;
    }

    fn on_chunk(&mut self, session: &str, attempt_id: &str, seq: u64, chunk: Value) {
        let Some(job) = self.job_for(session, attempt_id) else { return };
        if seq != job.next_seq {
            let job_id = job.new.id.clone();
            self.drop_job(&job_id, Some(ApiError::miner_failed("miner sent stream chunks out of order")));
            return;
        }
        job.next_seq += 1;
        job.committed = true;
        let chunk = shape_chunk(job, chunk);
        match job.events.try_send(JobEvent::Chunk(chunk)) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                let job_id = job.new.id.clone();
                warn!(job = %job_id, "client is not reading the stream; cancelling");
                self.drop_job(&job_id, None);
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                let job_id = job.new.id.clone();
                self.drop_job(&job_id, None);
            }
        }
    }

    fn on_job_error(&mut self, job_id: &str, session: &str, code: JobErrorCode, message: String) {
        let job = self.jobs.get_mut(job_id).expect("released job exists");
        let retry = code.retryable() && !job.committed && job.attempts <= MAX_RETRIES && Instant::now() < job.deadline;
        if retry {
            job.excluded.insert(session.into());
            job.state = JobState::Queued;
            self.queue.push_front(job_id.into());
            return;
        }
        let error = match code {
            JobErrorCode::InvalidRequest => ApiError::bad_request("invalid_request", message),
            JobErrorCode::ContextLengthExceeded => ApiError::bad_request("context_length_exceeded", message),
            JobErrorCode::Timeout => ApiError::new(StatusCode::GATEWAY_TIMEOUT, "inference_timeout", message),
            JobErrorCode::RuntimeFailed | JobErrorCode::Internal => ApiError::miner_failed(message),
        };
        self.fail(job_id, error);
    }

    fn requeue_rejected(&mut self, job_id: &str, session: &str, reason: RejectReason) {
        let job = self.jobs.get_mut(job_id).expect("released job exists");
        job.rejections += 1;
        // the dispatch never ran, so it does not use up the retry
        job.attempts = job.attempts.saturating_sub(1);
        if job.rejections > MAX_REJECTIONS {
            self.fail(job_id, ApiError::miner_failed(format!("miners kept rejecting the job ({reason:?})")));
            return;
        }
        job.excluded.insert(session.into());
        job.state = JobState::Queued;
        self.queue.push_front(job_id.into());
    }

    // ---- bookkeeping ----------------------------------------------------------------------

    fn job_for(&mut self, session: &str, attempt_id: &str) -> Option<&mut Job> {
        let job_id = self.miners.get(session)?.attempts.get(attempt_id)?.job.clone()?;
        let job = self.jobs.get_mut(&job_id)?;
        matches!(&job.state, JobState::Dispatched { attempt, .. } if attempt == attempt_id).then_some(job)
    }

    /// Frees the attempt's slot; returns its job if that job is still waiting on it.
    fn release(&mut self, session: &str, attempt_id: &str) -> Option<String> {
        let attempt = self.miners.get_mut(session)?.attempts.remove(attempt_id)?;
        let job_id = attempt.job?;
        let job = self.jobs.get(&job_id)?;
        matches!(&job.state, JobState::Dispatched { attempt, .. } if attempt == attempt_id).then_some(job_id)
    }

    fn fail(&mut self, job_id: &str, error: ApiError) {
        self.drop_job(job_id, Some(error));
    }

    /// Removes a job. A dispatched attempt is cancelled on its miner, keeping the
    /// slot reserved until the miner confirms. `error` is sent to a waiting client.
    fn drop_job(&mut self, job_id: &str, error: Option<ApiError>) {
        let Some(job) = self.jobs.remove(job_id) else { return };
        match &job.state {
            JobState::Queued => self.queue.retain(|id| id != job_id),
            JobState::Dispatched { session, attempt, .. } => {
                // only an attempt the miner still runs needs cancelling
                if let Some(miner) = self.miners.get_mut(session)
                    && let Some(held) = miner.attempts.get_mut(attempt)
                {
                    held.job = None;
                    let _ = miner.outbound.try_send(PoolMessage::Cancel { attempt_id: attempt.clone() });
                }
            }
        }
        if let Some(error) = error {
            let _ = job.events.try_send(JobEvent::Failed(error.with_request_id(job_id)));
        }
    }

    fn remove_miner(&mut self, session: &str, reason: &str) {
        let Some(miner) = self.miners.remove(session) else { return };
        info!(%session, label = %miner.label, %reason, "miner removed");
        for (attempt_id, attempt) in miner.attempts {
            let Some(job_id) = attempt.job else { continue };
            let Some(job) = self.jobs.get_mut(&job_id) else { continue };
            if !matches!(&job.state, JobState::Dispatched { attempt, .. } if *attempt == attempt_id) {
                continue;
            }
            if !job.committed && job.attempts <= MAX_RETRIES && Instant::now() < job.deadline {
                job.excluded.insert(session.into());
                job.state = JobState::Queued;
                self.queue.push_front(job_id);
            } else {
                self.fail(&job_id, ApiError::miner_failed(format!("{reason} during inference")));
            }
        }
        let orphaned: Vec<String> =
            self.counts.iter().filter(|(_, count)| count.session == session).map(|(id, _)| id.clone()).collect();
        for count_id in orphaned {
            let pending = self.counts.remove(&count_id).expect("listed above");
            let excluded = HashSet::from([session.to_owned()]);
            self.send_count(pending.request, pending.reply, pending.deadline, &excluded);
        }
        self.pump();
    }

    fn expire(&mut self, now: Instant) {
        let liveness = self.config.heartbeat * 3;
        let silent: Vec<String> = self
            .miners
            .iter()
            .filter(|(_, miner)| now.duration_since(miner.last_seen) > liveness)
            .map(|(session, _)| session.clone())
            .collect();
        for session in silent {
            self.remove_miner(&session, "miner stopped sending heartbeats");
        }

        let expired_counts: Vec<String> =
            self.counts.iter().filter(|(_, count)| now >= count.deadline).map(|(id, _)| id.clone()).collect();
        for count_id in expired_counts {
            let error = ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "count_timeout", "token counting timed out");
            self.finish_count(&count_id, Err(error.retry_after(2)));
        }

        let ack_timeout = self.config.dispatch_ack_timeout;
        let mut failures = Vec::new();
        let mut unacknowledged = Vec::new();
        for (id, job) in &self.jobs {
            match &job.state {
                _ if now >= job.deadline => failures.push((
                    id.clone(),
                    ApiError::new(StatusCode::GATEWAY_TIMEOUT, "inference_timeout", "the request deadline expired"),
                )),
                JobState::Queued if now >= job.queue_deadline => failures.push((
                    id.clone(),
                    ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "queue_timeout", "no slot became free in time")
                        .retry_after(5),
                )),
                JobState::Dispatched { session, attempt, sent_at, accepted: false, .. }
                    if now.duration_since(*sent_at) > ack_timeout =>
                {
                    unacknowledged.push((id.clone(), session.clone(), attempt.clone()));
                }
                JobState::Dispatched { .. }
                    if job.new.request.stream && !job.committed && now >= job.first_chunk_deadline =>
                {
                    failures.push((
                        id.clone(),
                        ApiError::new(
                            StatusCode::GATEWAY_TIMEOUT,
                            "inference_timeout",
                            "the model did not start answering in time",
                        ),
                    ))
                }
                _ => {}
            }
        }
        for (job_id, error) in failures {
            self.fail(&job_id, error);
        }
        for (job_id, session, attempt) in unacknowledged {
            warn!(job = %job_id, %session, "miner did not acknowledge the job; retrying elsewhere");
            if let Some(miner) = self.miners.get_mut(&session) {
                if let Some(held) = miner.attempts.get_mut(&attempt) {
                    held.job = None;
                }
                let _ = miner.outbound.try_send(PoolMessage::Cancel { attempt_id: attempt });
            }
            self.on_job_error(&job_id, &session, JobErrorCode::Timeout, "miner did not acknowledge the job".into());
        }
        self.pump();
    }

    fn snapshot(&self) -> Snapshot {
        let now = Instant::now();
        let mut models: HashMap<String, ModelAvailability> =
            self.catalog.models.iter().map(|model| (model.id.clone(), ModelAvailability::default())).collect();
        for miner in self.miners.values().filter(|miner| !miner.draining) {
            let mut seen = HashSet::new();
            for loaded in &miner.models {
                let Some(entry) = models.get_mut(&loaded.model) else { continue };
                if seen.insert(&loaded.model) {
                    entry.ready_miners += 1;
                }
                entry.max_available_context_tokens = entry.max_available_context_tokens.max(loaded.context_tokens);
                if miner.busy(&loaded.device) < miner.slots(&loaded.device) {
                    entry.free_slots += 1;
                    entry.max_idle_context_tokens = entry.max_idle_context_tokens.max(loaded.context_tokens);
                }
            }
        }
        for job_id in &self.queue {
            if let Some(entry) = models.get_mut(&self.jobs[job_id].new.model) {
                entry.queued_requests += 1;
            }
        }
        let mut miners: Vec<MinerStatus> = self
            .miners
            .iter()
            .map(|(session, miner)| MinerStatus {
                session_id: session.clone(),
                label: miner.label.clone(),
                miner_id: miner.miner_id.clone(),
                hardware: miner.hardware.clone(),
                devices: miner
                    .devices
                    .iter()
                    .map(|d| DeviceStatus { id: d.id.clone(), slots: d.slots, busy: miner.busy(&d.id) })
                    .collect(),
                models: miner.models.clone(),
                draining: miner.draining,
                seconds_since_seen: now.duration_since(miner.last_seen).as_secs(),
            })
            .collect();
        miners.sort_by(|a, b| a.session_id.cmp(&b.session_id));
        Snapshot {
            models,
            miners,
            queued_jobs: self.queue.len(),
            active_jobs: self.jobs.len() - self.queue.len(),
            pending_counts: self.counts.len(),
        }
    }
}

/// Client-facing names: the API model id and a stable completion id, never the
/// runtime's file name or internal ids.
fn shape_response(job: &Job, mut body: Value) -> Value {
    if let Some(object) = body.as_object_mut() {
        object.insert("model".into(), job.new.model.clone().into());
        if job.new.request.operation == Operation::ChatCompletions {
            object.insert("id".into(), completion_id(job).into());
            object.insert("created".into(), job.created_unix.into());
            object.remove("timings");
        }
    }
    body
}

fn shape_chunk(job: &Job, mut chunk: Value) -> Value {
    if let Some(object) = chunk.as_object_mut() {
        object.insert("id".into(), completion_id(job).into());
        object.insert("model".into(), job.new.model.clone().into());
        object.insert("created".into(), job.created_unix.into());
        object.remove("timings");
    }
    chunk
}

fn completion_id(job: &Job) -> String {
    format!("chatcmpl-{}", job.new.id.trim_start_matches("req_"))
}
