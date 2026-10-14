//! The scheduler: a single task that owns miners, device slots, the job queue
//! and pending token counts. HTTP handlers and miner connections talk to it
//! through commands, so slot reservation, cancellation and retries never race.
//!
//! Guarantees: a client gets at most one response. An attempt is retried once,
//! on another miner, only before anything was committed to the client. A slot
//! stays reserved until the miner confirms the work stopped or disconnects.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{
        Arc, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicUsize, Ordering},
    },
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
    sync::{mpsc, oneshot, watch},
    time::{Instant, MissedTickBehavior},
};
use tracing::{debug, info, warn};

use crate::{
    config::Config,
    error::ApiError,
    stats::{Stats, StatsSnapshot},
};

/// Infrastructure retries after the first attempt.
const MAX_RETRIES: u32 = 1;
/// Rejections (e.g. a miner that was busy after all) before giving up.
const MAX_REJECTIONS: u32 = 3;
const MAX_PENDING_COUNTS: usize = 256;
/// Completions older than this do not count towards the observed throughput.
const RATE_WINDOW: Duration = Duration::from_secs(60);
const MAX_RETRY_AFTER: u64 = 60;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Admission state shared between the HTTP handlers and the scheduler, so a
/// request can be refused before its body is read, parsed or counted.
pub struct Admission {
    /// `max_active + max_queued` per client; `None` means no per-client limit.
    per_client: Option<usize>,
    /// Requests per client anywhere in the pipeline (parsing, counting, queued, running).
    inflight: Mutex<HashMap<String, usize>>,
    /// Published by the scheduler after every command.
    queue_len: AtomicUsize,
    queue_capacity: AtomicUsize,
    /// Recent job completions (any outcome), for Retry-After.
    completions: Mutex<VecDeque<(Instant, String)>>,
}

/// One client's slot in the pipeline; released when dropped, on every exit path.
pub struct Reservation {
    admission: Arc<Admission>,
    client: Option<String>,
}

impl Admission {
    /// `per_client`: the most requests one client may have in the pipeline, if limited.
    pub fn new(per_client: Option<usize>) -> Arc<Self> {
        Arc::new(Self {
            per_client,
            inflight: Mutex::default(),
            queue_len: AtomicUsize::new(0),
            queue_capacity: AtomicUsize::new(usize::MAX),
            completions: Mutex::default(),
        })
    }

    /// Takes a slot for `client`, or fails with the error the request would get anyway.
    pub fn reserve(self: &Arc<Self>, client: &str) -> Result<Reservation, ApiError> {
        let (len, capacity) = (self.queue_len.load(Ordering::Relaxed), self.queue_capacity.load(Ordering::Relaxed));
        if len >= capacity {
            return Err(self.overloaded(len - capacity + 1));
        }
        let Some(limit) = self.per_client else {
            return Ok(Reservation { admission: self.clone(), client: None });
        };
        let mut inflight = lock(&self.inflight);
        let used = inflight.entry(client.into()).or_insert(0);
        if *used >= limit {
            let ahead = *used - limit + 1;
            drop(inflight);
            return Err(self.rate_limited(client, limit, ahead));
        }
        *used += 1;
        Ok(Reservation { admission: self.clone(), client: Some(client.into()) })
    }

    pub fn overloaded(&self, ahead: usize) -> ApiError {
        ApiError::overloaded("the pool queue is full").retry_after(retry_after(ahead, self.completed(None)))
    }

    pub fn rate_limited(&self, client: &str, limit: usize, ahead: usize) -> ApiError {
        // a slot frees when one of the client's own jobs ends; fall back to the pool's rate
        let own = self.completed(Some(client));
        let done = if own > 0 { own } else { self.completed(None) };
        ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limit_exceeded",
            format!("at most {limit} requests may run or wait per client"),
        )
        .retry_after(retry_after(ahead, done))
    }

    fn record_completion(&self, client: &str) {
        let now = Instant::now();
        let mut completions = lock(&self.completions);
        completions.push_back((now, client.into()));
        Self::prune(&mut completions, now);
    }

    /// Completions within [`RATE_WINDOW`], by one client or by everyone.
    fn completed(&self, client: Option<&str>) -> usize {
        let mut completions = lock(&self.completions);
        Self::prune(&mut completions, Instant::now());
        completions.iter().filter(|(_, who)| client.is_none_or(|c| c == who)).count()
    }

    fn prune(completions: &mut VecDeque<(Instant, String)>, now: Instant) {
        while completions.front().is_some_and(|(at, _)| now.duration_since(*at) > RATE_WINDOW) {
            completions.pop_front();
        }
    }
}

/// Seconds until `ahead` jobs have probably finished, from the completions seen in
/// the last [`RATE_WINDOW`]: ceil(ahead / (done / window)), clamped to [1, 60].
/// Nothing completed recently means no usable rate, so the maximum.
fn retry_after(ahead: usize, done: usize) -> u64 {
    if done == 0 {
        return MAX_RETRY_AFTER;
    }
    let seconds = (ahead as u64 * RATE_WINDOW.as_secs()).div_ceil(done as u64);
    seconds.clamp(1, MAX_RETRY_AFTER)
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let Some(client) = self.client.take() else { return };
        let mut inflight = lock(&self.admission.inflight);
        if let Some(used) = inflight.get_mut(&client) {
            *used -= 1;
            if *used == 0 {
                inflight.remove(&client);
            }
        }
    }
}

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

impl NewJob {
    fn routing_context(&self) -> u32 {
        self.required_context.max(self.request.min_context_tokens.unwrap_or(0))
    }
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
    admission: Arc<Admission>,
    stats: watch::Receiver<StatsSnapshot>,
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

    /// Reserves the client's place in the pipeline; call before reading the body.
    pub fn reserve(&self, client: &str) -> Result<Reservation, ApiError> {
        self.admission.reserve(client)
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

    /// Reads published public totals without sending a scheduler command.
    pub fn stats(&self) -> StatsSnapshot {
        self.stats.borrow().clone()
    }

    pub async fn snapshot(&self) -> Snapshot {
        let (reply, response) = oneshot::channel();
        self.send(Command::Snapshot { reply });
        response.await.unwrap_or_default()
    }
}

/// Cancels its job when dropped before [`JobGuard::finish`], e.g. when the
/// client disconnects and axum drops the handler or the response stream.
/// It also owns the client's [`Reservation`] for as long as the response lives.
pub struct JobGuard {
    handle: SchedulerHandle,
    job_id: String,
    finished: bool,
    _reservation: Reservation,
}

impl JobGuard {
    pub fn new(handle: SchedulerHandle, job_id: &str, reservation: Reservation) -> Self {
        Self { handle, job_id: job_id.into(), finished: false, _reservation: reservation }
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

/// Incremental job counts, so admission and dispatch never scan every job.
/// `clients` maps a client to (dispatched, queued) jobs, `models` maps a model
/// to its queued jobs; entries are removed when they reach zero.
#[derive(Default)]
struct Tally {
    clients: HashMap<String, (usize, usize)>,
    models: HashMap<String, usize>,
    queued_questions: u64,
}

impl Tally {
    fn client(&self, client: &str) -> (usize, usize) {
        self.clients.get(client).copied().unwrap_or((0, 0))
    }

    fn model_queued(&self, model: &str) -> usize {
        self.models.get(model).copied().unwrap_or(0)
    }

    /// A job entered the queue (admitted, or sent back to it).
    fn enqueue(&mut self, client: &str, model: &str, questions: u64) {
        self.queued_questions += questions;
        self.clients.entry(client.to_owned()).or_default().1 += 1;
        *self.models.entry(model.to_owned()).or_default() += 1;
    }

    /// A queued job left the queue without being tracked any further.
    fn dequeue(&mut self, client: &str, model: &str, questions: u64) {
        self.queued_questions -= questions;
        let entry = self.clients.get_mut(client).expect("client has a queued job");
        entry.1 -= 1;
        if *entry == (0, 0) {
            self.clients.remove(client);
        }
        let queued = self.models.get_mut(model).expect("model has a queued job");
        *queued -= 1;
        if *queued == 0 {
            self.models.remove(model);
        }
    }

    /// A queued job was sent to a miner.
    fn activate(&mut self, client: &str, model: &str, questions: u64) {
        self.dequeue(client, model, questions);
        self.clients.entry(client.to_owned()).or_default().0 += 1;
    }

    /// A dispatched job went back to the queue.
    fn deactivate(&mut self, client: &str, model: &str, questions: u64) {
        self.release_active(client);
        self.enqueue(client, model, questions);
    }

    fn release_active(&mut self, client: &str) {
        let entry = self.clients.get_mut(client).expect("client has an active job");
        entry.0 -= 1;
        if *entry == (0, 0) {
            self.clients.remove(client);
        }
    }

    /// A job is being removed from the scheduler in its current state.
    fn forget(&mut self, job: &Job) {
        match job.state {
            JobState::Queued => self.dequeue(&job.new.client, &job.new.model, job.new.request.question_count),
            JobState::Dispatched { .. } => self.release_active(&job.new.client),
        }
    }
}

pub struct Scheduler {
    config: Config,
    catalog: Arc<Catalog>,
    miners: HashMap<String, Miner>,
    jobs: HashMap<String, Job>,
    queue: VecDeque<String>,
    tally: Tally,
    counts: HashMap<String, PendingCount>,
    rotation: usize,
    admission: Arc<Admission>,
    stats: Stats,
    public_stats: watch::Sender<StatsSnapshot>,
}

pub fn spawn(config: Config, catalog: Arc<Catalog>) -> SchedulerHandle {
    let (commands, receiver) = mpsc::unbounded_channel();
    let per_client = match (config.max_active_per_client, config.max_queued_per_client) {
        (Some(active), Some(queued)) => Some(active + queued),
        _ => None,
    };
    let admission = Admission::new(per_client);
    let stats = Stats::new(Instant::now(), SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default());
    let (public_stats, stats_receiver) = watch::channel(stats.snapshot(0, 0));
    let scheduler = Scheduler {
        stats,
        public_stats,
        admission: admission.clone(),
        config,
        catalog,
        miners: HashMap::new(),
        jobs: HashMap::new(),
        queue: VecDeque::new(),
        tally: Tally::default(),
        counts: HashMap::new(),
        rotation: 0,
    };
    tokio::spawn(scheduler.run(receiver));
    SchedulerHandle { commands, admission, stats: stats_receiver }
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
            self.publish();
        }
    }

    /// Shows the queue fill to the handlers, which refuse early when it is full.
    fn publish(&self) {
        self.public_stats.send_replace(self.stats.snapshot(self.tally.queued_questions, self.connected_miners()));
        self.admission.queue_len.store(self.queue.len(), Ordering::Relaxed);
        self.admission.queue_capacity.store(self.queue_capacities("").0, Ordering::Relaxed);
    }

    fn connected_miners(&self) -> usize {
        self.miners.values().filter(|miner| !miner.draining).count()
    }

    fn advance_stats(&mut self, now: Instant) {
        self.stats.advance(now, self.tally.queued_questions, self.connected_miners());
    }

    fn handle(&mut self, command: Command) {
        self.advance_stats(Instant::now());
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
        if !self
            .miners
            .values()
            .any(|miner| !miner.draining && miner.serves(&new.model, &new.model_revision).next().is_some())
        {
            return Err(ApiError::no_miner(&new.model));
        }
        let (client_active, client_queued) = self.tally.client(&new.client);
        let client_jobs = client_active + client_queued;
        if let (Some(active), Some(queued)) = (self.config.max_active_per_client, self.config.max_queued_per_client)
            && client_jobs >= active + queued
        {
            return Err(self.admission.rate_limited(&new.client, active + queued, 1));
        }
        let model_queued = self.tally.model_queued(&new.model);
        let (global_capacity, model_capacity) = self.queue_capacities(&new.model);
        if self.queue.len() >= global_capacity || model_queued >= model_capacity {
            return Err(self.admission.overloaded(self.queue.len().saturating_sub(global_capacity) + 1));
        }

        let now = Instant::now();
        let id = new.id.clone();
        debug!(job = %id, required_context = new.required_context, routing_context = new.routing_context(), "queued");
        self.tally.enqueue(&new.client, &new.model, new.request.question_count);
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

    /// Queue capacities `(global, for the model)`: the fixed operator values, or
    /// 4 x the slots of the connected, non-draining miners (at least 16).
    fn queue_capacities(&self, model: &str) -> (usize, usize) {
        let mut all = 0usize;
        let mut serving = 0usize;
        for miner in self.miners.values().filter(|miner| !miner.draining) {
            all += miner.devices.iter().map(|d| d.slots as usize).sum::<usize>();
            let mut devices: Vec<&str> =
                miner.models.iter().filter(|m| m.model == model).map(|m| m.device.as_str()).collect();
            devices.sort_unstable();
            devices.dedup();
            serving += devices.into_iter().map(|device| miner.slots(device) as usize).sum::<usize>();
        }
        (
            self.config.queue_capacity_global.unwrap_or_else(|| (4 * all).max(16)),
            self.config.queue_capacity_per_model.unwrap_or_else(|| (4 * serving).max(16)),
        )
    }

    /// Dispatches every queued job that has a fitting free slot, oldest first.
    /// Queue order is strictly FIFO; without per-client limits a client that
    /// queued earlier is served earlier, and there is no other fairness rule.
    /// A job that cannot be placed does not block smaller ones behind it.
    fn pump(&mut self) {
        let mut index = 0;
        while index < self.queue.len() {
            let job_id = self.queue[index].clone();
            let job = &self.jobs[&job_id];
            let client_active = self.tally.client(&job.new.client).0;
            if self.config.max_active_per_client.is_some_and(|max| client_active >= max) {
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
        self.debug_check();
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
                if loaded.context_tokens >= job.new.routing_context() && busy < miner.slots(&loaded.device) {
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
        debug!(job = %job_id, %attempt, %session, %profile, routing_context = job.new.routing_context(), "dispatched");
        job.attempts += 1;
        self.tally.activate(&job.new.client, &job.new.model, job.new.request.question_count);
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
                    let job = self.remove_job(&job_id).expect("released job exists");
                    let profile = match &job.state {
                        JobState::Dispatched { profile, .. } => profile.clone(),
                        JobState::Queued => String::new(),
                    };
                    let body = shape_response(&job, body);
                    if job.events.try_send(JobEvent::Result { body, profile }).is_ok() {
                        self.stats.answered(job.new.request.question_count);
                    }
                }
            }
            MinerMessage::StreamStart { .. } => {}
            MinerMessage::StreamChunk { attempt_id, seq, chunk } => self.on_chunk(session, &attempt_id, seq, chunk),
            MinerMessage::StreamEnd { attempt_id, usage } => {
                if let Some(job_id) = self.release(session, &attempt_id) {
                    let job = self.remove_job(&job_id).expect("released job exists");
                    if job.new.request.include_usage
                        && let Some(usage) = usage
                    {
                        let chunk = shape_chunk(
                            &job,
                            json!({"object": "chat.completion.chunk", "choices": [], "usage": usage}),
                        );
                        let _ = job.events.try_send(JobEvent::Chunk(chunk));
                    }
                    if job.events.try_send(JobEvent::End).is_ok() {
                        self.stats.answered(job.new.request.question_count);
                    }
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

    /// Whether a failed attempt may run again: nothing was committed, the retry
    /// is unused, time remains, and some other miner could take the job.
    /// Without another miner the client gets the real error now instead of
    /// waiting out the queue timeout.
    fn can_retry(&self, job_id: &str, failed_session: &str) -> bool {
        let job = &self.jobs[job_id];
        !job.committed
            && job.attempts <= MAX_RETRIES
            && Instant::now() < job.deadline
            && self.miners.iter().any(|(session, miner)| {
                session != failed_session
                    && !job.excluded.contains(session)
                    && !miner.draining
                    && miner
                        .serves(&job.new.model, &job.new.model_revision)
                        .any(|loaded| loaded.context_tokens >= job.new.routing_context())
            })
    }

    fn on_job_error(&mut self, job_id: &str, session: &str, code: JobErrorCode, message: String) {
        let retry = code.retryable() && self.can_retry(job_id, session);
        let job = self.jobs.get_mut(job_id).expect("released job exists");
        if retry {
            job.excluded.insert(session.into());
            job.state = JobState::Queued;
            self.tally.deactivate(&job.new.client, &job.new.model, job.new.request.question_count);
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
        let rejections = job.rejections;
        if rejections > MAX_REJECTIONS || !self.can_retry(job_id, session) {
            self.fail(
                job_id,
                ApiError::miner_failed(format!("no miner could take the job (last rejection: {reason:?})")),
            );
            return;
        }
        let job = self.jobs.get_mut(job_id).expect("released job exists");
        job.excluded.insert(session.into());
        job.state = JobState::Queued;
        self.tally.deactivate(&job.new.client, &job.new.model, job.new.request.question_count);
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

    /// The single place a job leaves `self.jobs`: forgets its tally and counts it
    /// towards the completion rate that drives Retry-After.
    fn remove_job(&mut self, job_id: &str) -> Option<Job> {
        let job = self.jobs.remove(job_id)?;
        self.tally.forget(&job);
        self.admission.record_completion(&job.new.client);
        Some(job)
    }

    /// Removes a job. A dispatched attempt is cancelled on its miner, keeping the
    /// slot reserved until the miner confirms. `error` is sent to a waiting client.
    fn drop_job(&mut self, job_id: &str, error: Option<ApiError>) {
        let Some(job) = self.remove_job(job_id) else { return };
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
            if self.can_retry(&job_id, session) {
                let job = self.jobs.get_mut(&job_id).expect("checked above");
                job.excluded.insert(session.into());
                job.state = JobState::Queued;
                self.tally.deactivate(&job.new.client, &job.new.model, job.new.request.question_count);
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
        self.advance_stats(now);
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

    /// Debug-only: the counters must agree with the job table and the queue.
    fn debug_check(&self) {
        #[cfg(debug_assertions)]
        {
            let (active, queued) = self.tally.clients.values().fold((0, 0), |a, c| (a.0 + c.0, a.1 + c.1));
            debug_assert_eq!(queued, self.queue.len(), "queued counter drifted");
            debug_assert_eq!(
                self.tally.queued_questions,
                self.jobs
                    .values()
                    .filter(|job| matches!(job.state, JobState::Queued))
                    .map(|job| job.new.request.question_count)
                    .sum::<u64>(),
                "queued question counter drifted"
            );
            debug_assert_eq!(active + queued, self.jobs.len(), "job counter drifted");
            debug_assert_eq!(self.tally.models.values().sum::<usize>(), queued, "model counter drifted");
            debug_assert!(self.tally.clients.values().all(|c| *c != (0, 0)));
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use pool_protocol::{Accelerator, requests};

    fn config() -> Config {
        Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            catalog: "config/models.json".into(),
            auth_mode: crate::config::AuthMode::Local,
            client_api_keys: vec![],
            miner_tokens: vec![],
            miner_auth: crate::config::MinerAuth::Token,
            max_miners: 256,
            admin_token: None,
            request_timeout: Duration::from_secs(20),
            count_timeout: Duration::from_secs(5),
            queue_timeout: Duration::from_secs(5),
            dispatch_ack_timeout: Duration::from_secs(3),
            first_chunk_timeout: Duration::from_secs(10),
            queue_capacity_global: Some(64),
            queue_capacity_per_model: Some(32),
            max_active_per_client: Some(4),
            max_queued_per_client: Some(8),
            heartbeat: Duration::from_secs(10),
            stream_buffer_events: 64,
        }
    }

    fn scheduler() -> Scheduler {
        let mut config = config();
        config.queue_capacity_global = None;
        config.queue_capacity_per_model = None;
        let stats = Stats::new(Instant::now(), SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default());
        let (public_stats, _) = watch::channel(stats.snapshot(0, 0));
        Scheduler {
            stats,
            public_stats,
            config,
            catalog: Arc::new(Catalog::parse(include_str!("../../../config/models.json")).unwrap()),
            miners: HashMap::new(),
            jobs: HashMap::new(),
            queue: VecDeque::new(),
            tally: Tally::default(),
            counts: HashMap::new(),
            rotation: 0,
            admission: Admission::new(Some(12)),
        }
    }

    fn miner(scheduler: &mut Scheduler, session: &str, profile: &str) -> mpsc::Receiver<PoolMessage> {
        miner_model(scheduler, session, "clef", profile)
    }

    fn miner_model(
        scheduler: &mut Scheduler,
        session: &str,
        model_id: &str,
        profile: &str,
    ) -> mpsc::Receiver<PoolMessage> {
        let (outbound, receiver) = mpsc::channel(32);
        scheduler.handle(Command::MinerConnected {
            session: session.into(),
            label: session.into(),
            miner_id: session.into(),
            outbound,
            hardware: Hardware {
                os: "test".into(),
                arch: "test".into(),
                accelerator: Accelerator::Cuda,
                devices: vec![],
            },
        });
        let model = scheduler.catalog.model(model_id).unwrap();
        let loaded = LoadedModel {
            model: model.id.clone(),
            model_revision: scheduler.catalog.model_revision(model),
            profile: profile.into(),
            device: "gpu0".into(),
            context_tokens: model.profile(profile).unwrap().context_tokens,
        };
        scheduler.on_miner_message(
            session,
            MinerMessage::Capacity { devices: vec![DeviceSlots { id: "gpu0".into(), slots: 1 }], models: vec![loaded] },
        );
        receiver
    }

    fn job(scheduler: &Scheduler, id: &str, counted: u32, minimum: Option<u32>) -> NewJob {
        let model = scheduler.catalog.model("clef").unwrap();
        let mut body = json!({"state": "short", "questions": {"q": {"type": "noul", "instructions": "x"}}});
        if let Some(minimum) = minimum {
            body["min_context_tokens"] = minimum.into();
        }
        NewJob {
            id: id.into(),
            client: "client".into(),
            model: model.id.clone(),
            model_revision: scheduler.catalog.model_revision(model),
            input_tokens: counted,
            required_context: counted,
            request: requests::validate(Operation::Systemone, model, body.as_object().unwrap().clone()).unwrap(),
        }
    }

    #[test]
    fn context_waits_skips_small_jobs_and_dispatches_on_capacity() {
        for (counted, minimum) in [(100, Some(8192)), (5000, None)] {
            let mut scheduler = scheduler();
            let mut small = miner(&mut scheduler, "small", "cuda-4096");
            let (events, _receiver) = mpsc::channel(1);
            scheduler.admit(job(&scheduler, "waiting", counted, minimum), events.clone()).unwrap();
            scheduler.pump();
            assert!(small.try_recv().is_err());
            assert_eq!(scheduler.queue.len(), 1);
            scheduler.admit(job(&scheduler, "short", 100, None), events).unwrap();
            scheduler.pump();
            assert!(matches!(small.try_recv().unwrap(), PoolMessage::Job { job_id, .. } if job_id == "short"));
            assert_eq!(scheduler.queue.len(), 1);
            let mut large = miner(&mut scheduler, "large", "cuda-16384");
            assert!(
                matches!(large.try_recv().unwrap(), PoolMessage::Job { job_id, required_context_tokens, .. } if job_id == "waiting" && required_context_tokens == counted)
            );
            assert!(scheduler.queue.is_empty());
            assert_eq!(scheduler.jobs["waiting"].new.routing_context(), counted.max(minimum.unwrap_or(0)));
        }
    }

    #[test]
    fn context_waits_and_times_out_with_or_without_a_minimum() {
        for minimum in [None, Some(1), Some(8192)] {
            let mut scheduler = scheduler();
            let mut small = miner(&mut scheduler, "small", "cuda-4096");
            let (events, mut receiver) = mpsc::channel(1);
            scheduler.admit(job(&scheduler, "waiting", 5000, minimum), events).unwrap();
            assert_eq!(scheduler.jobs["waiting"].new.routing_context(), 5000.max(minimum.unwrap_or(0)));
            scheduler.pump();
            assert!(small.try_recv().is_err());
            scheduler.expire(Instant::now() + scheduler.config.queue_timeout + Duration::from_millis(1));
            assert!(matches!(receiver.try_recv().unwrap(), JobEvent::Failed(error) if error.code == "queue_timeout"));
            assert!(scheduler.jobs.is_empty());
            scheduler.miners.clear();
            let (events, _receiver) = mpsc::channel(1);
            assert_eq!(
                scheduler.admit(job(&scheduler, "empty", 100, minimum), events).unwrap_err().code,
                "no_miner_available"
            );
        }
    }

    #[test]
    fn minimum_context_retries_require_another_sufficient_profile() {
        let mut scheduler = scheduler();
        let _small = miner(&mut scheduler, "small", "cuda-4096");
        let mut first = miner(&mut scheduler, "first", "cuda-8192");
        let (events, _receiver) = mpsc::channel(1);
        scheduler.admit(job(&scheduler, "retry", 100, Some(8192)), events).unwrap();
        scheduler.pump();
        assert!(matches!(first.try_recv().unwrap(), PoolMessage::Job { required_context_tokens: 100, .. }));
        assert!(!scheduler.can_retry("retry", "first"));
        let mut next = miner(&mut scheduler, "next", "cuda-16384");
        assert!(scheduler.can_retry("retry", "first"));
        scheduler.on_job_error("retry", "first", JobErrorCode::RuntimeFailed, "failed".into());
        scheduler.pump();
        assert!(
            matches!(next.try_recv().unwrap(), PoolMessage::Job { profile, required_context_tokens: 100, .. } if profile == "cuda-16384")
        );
    }

    #[test]
    fn waiting_minimum_jobs_use_normal_queue_capacity_and_admission() {
        let mut scheduler = scheduler();
        let _small = miner(&mut scheduler, "small", "cuda-4096");
        assert_eq!(scheduler.queue_capacities("clef"), (16, 16));
        scheduler.config.max_active_per_client = None;
        scheduler.config.max_queued_per_client = None;
        for index in 0..16 {
            let (events, _receiver) = mpsc::channel(1);
            scheduler.admit(job(&scheduler, &format!("wait{index}"), 100, Some(8192)), events).unwrap();
        }
        let (events, _receiver) = mpsc::channel(1);
        assert_eq!(
            scheduler.admit(job(&scheduler, "full", 100, Some(8192)), events).unwrap_err().code,
            "pool_overloaded"
        );
        scheduler.publish();
        assert_eq!(scheduler.admission.reserve("other").err().unwrap().code, "pool_overloaded");
    }

    #[test]
    fn minimum_context_dispatches_when_matching_capacity_frees() {
        let mut scheduler = scheduler();
        let mut large = miner(&mut scheduler, "large", "cuda-8192");
        let (events, _receiver) = mpsc::channel(2);
        scheduler.admit(job(&scheduler, "first", 100, Some(8192)), events.clone()).unwrap();
        scheduler.pump();
        let PoolMessage::Job { attempt_id, .. } = large.try_recv().unwrap() else { panic!("expected first job") };
        scheduler.admit(job(&scheduler, "second", 100, Some(8192)), events).unwrap();
        scheduler.pump();
        assert!(large.try_recv().is_err());
        assert_eq!(scheduler.queue.len(), 1);
        scheduler.on_miner_message(
            "large",
            MinerMessage::Result {
                attempt_id,
                body: json!({"answers": {"q": {"noul": 0.9}}, "usage": {"input_tokens": 100}}),
            },
        );
        assert!(matches!(large.try_recv().unwrap(), PoolMessage::Job { job_id, .. } if job_id == "second"));
    }

    #[test]
    fn reservations_are_capped_per_client_and_released_on_drop() {
        let admission = Admission::new(Some(2));
        let first = admission.reserve("a").unwrap();
        let _second = admission.reserve("a").unwrap();
        assert_eq!(admission.reserve("a").err().unwrap().code, "rate_limit_exceeded");
        // other clients are unaffected
        let _other = admission.reserve("b").unwrap();
        drop(first);
        let _again = admission.reserve("a").unwrap();
        assert!(admission.reserve("a").is_err());
    }

    #[test]
    fn dropping_every_reservation_empties_the_table() {
        let admission = Admission::new(Some(1));
        drop(admission.reserve("a").unwrap());
        assert!(admission.inflight.lock().unwrap().is_empty());
    }

    #[test]
    fn unlimited_clients_are_never_refused_but_a_full_queue_is() {
        let admission = Admission::new(None);
        let held: Vec<_> = (0..100).map(|_| admission.reserve("a").unwrap()).collect();
        assert_eq!(held.len(), 100);
        admission.queue_capacity.store(4, std::sync::atomic::Ordering::Relaxed);
        admission.queue_len.store(4, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(admission.reserve("a").err().unwrap().code, "pool_overloaded");
    }

    #[test]
    fn retry_after_follows_throughput_and_is_clamped() {
        assert_eq!(retry_after(1, 0), 60);
        assert_eq!(retry_after(1, 600), 1);
        assert_eq!(retry_after(1, 30), 2);
        assert_eq!(retry_after(10, 60), 10);
        assert_eq!(retry_after(500, 10), 60);
    }

    #[test]
    fn tally_follows_a_job_and_drops_empty_entries() {
        let mut tally = Tally::default();
        tally.enqueue("c", "m", 3);
        tally.enqueue("c", "m", 3);
        assert_eq!(tally.queued_questions, 6);
        assert_eq!(tally.client("c"), (0, 2));
        assert_eq!(tally.model_queued("m"), 2);
        tally.activate("c", "m", 3);
        assert_eq!(tally.queued_questions, 3);
        assert_eq!((tally.client("c"), tally.model_queued("m")), ((1, 1), 1));
        tally.deactivate("c", "m", 3);
        assert_eq!(tally.queued_questions, 6);
        assert_eq!((tally.client("c"), tally.model_queued("m")), ((0, 2), 2));
        tally.activate("c", "m", 3);
        tally.release_active("c");
        tally.dequeue("c", "m", 3);
        assert_eq!(tally.queued_questions, 0);
        assert!(tally.clients.is_empty() && tally.models.is_empty());
    }

    #[test]
    fn public_stats_sample_waiting_questions_and_exclude_draining_miners() {
        let mut scheduler = scheduler();
        let now = Instant::now();
        scheduler.stats = Stats::new(now, Duration::from_secs(120));
        let _small = miner(&mut scheduler, "small", "cuda-4096");
        let _draining = miner(&mut scheduler, "draining", "cuda-4096");
        scheduler.on_miner_message("draining", MinerMessage::Draining);
        let (events, _receiver) = mpsc::channel(1);
        let mut waiting = job(&scheduler, "waiting", 100, Some(8192));
        waiting.request.question_count = 3;
        scheduler.admit(waiting, events).unwrap();
        scheduler.pump();
        scheduler.advance_stats(now + Duration::from_secs(60));
        scheduler.publish();
        assert!(scheduler.public_stats.borrow().history.answered.is_empty());
        scheduler.advance_stats(now + Duration::from_secs(120));
        scheduler.publish();
        let snapshot = scheduler.public_stats.borrow().clone();
        assert_eq!(snapshot.queued_questions, 3);
        assert_eq!(snapshot.miners, 1);
        assert_eq!(snapshot.history.queued, [3]);
        assert_eq!(snapshot.history.miners, [1]);
        scheduler.drop_job("waiting", None);
        scheduler.publish();
        assert_eq!(scheduler.public_stats.borrow().queued_questions, 0);
    }

    #[test]
    fn only_successful_jobs_count_once_after_a_retry() {
        let mut scheduler = scheduler();
        let now = Instant::now();
        scheduler.stats = Stats::new(now, Duration::from_secs(120));
        scheduler.advance_stats(now + Duration::from_secs(60));
        let mut first = miner(&mut scheduler, "first", "cuda-8192");
        let (events, _receiver) = mpsc::channel(1);
        let mut request = job(&scheduler, "retry", 100, None);
        request.request.question_count = 3;
        scheduler.admit(request, events).unwrap();
        scheduler.pump();
        let PoolMessage::Job { attempt_id: failed_attempt, .. } = first.try_recv().unwrap() else {
            panic!("expected first attempt")
        };
        let mut second = miner(&mut scheduler, "second", "cuda-8192");
        scheduler.on_miner_message(
            "first",
            MinerMessage::JobError {
                attempt_id: failed_attempt,
                code: JobErrorCode::RuntimeFailed,
                message: "failed".into(),
            },
        );
        assert_eq!(scheduler.stats.snapshot(0, 0).answered_per_second, 0.0);
        let PoolMessage::Job { attempt_id, .. } = second.try_recv().unwrap() else { panic!("expected retry") };
        let result = MinerMessage::Result { attempt_id, body: json!({"answers": {}}) };
        scheduler.on_miner_message("second", result.clone());
        scheduler.on_miner_message("second", result);
        assert_eq!(scheduler.stats.snapshot(0, 0).answered_per_second, 3.0 / 60.0);
        scheduler.advance_stats(now + Duration::from_secs(120));
        let snapshot = scheduler.stats.snapshot(0, 0);
        assert_eq!(snapshot.answered_last_24h, 3);
        assert_eq!(snapshot.history.answered, [3]);
    }

    #[test]
    fn terminal_success_counts_only_when_enqueued_in_the_client_channel() {
        for (stream, include_usage, full, closed) in [
            (false, false, false, false),
            (false, false, false, true),
            (true, false, false, false),
            (true, false, false, true),
            (true, false, true, false),
            (true, true, false, false),
            (true, true, true, false),
        ] {
            let mut scheduler = scheduler();
            let now = Instant::now();
            scheduler.stats = Stats::new(now, Duration::from_secs(120));
            scheduler.advance_stats(now + Duration::from_secs(60));
            let mut outbound = miner_model(&mut scheduler, "miner", "qwen2.5-1.5b-instruct", "cuda-4096");
            let model = scheduler.catalog.model("qwen2.5-1.5b-instruct").unwrap();
            let body = json!({"messages": [{"role": "user", "content": "hi"}], "stream": stream,
                "stream_options": {"include_usage": include_usage}});
            let mut fields = body.as_object().unwrap().clone();
            if !stream {
                fields.remove("stream_options");
            }
            let request = requests::validate(Operation::ChatCompletions, model, fields).unwrap();
            let job = NewJob {
                id: "terminal".into(),
                client: "client".into(),
                model: model.id.clone(),
                model_revision: scheduler.catalog.model_revision(model),
                required_context: request.required_context(100),
                input_tokens: 100,
                request,
            };
            let (events, receiver) = mpsc::channel(1);
            let mut receiver = Some(receiver);
            scheduler.admit(job, events).unwrap();
            scheduler.pump();
            let PoolMessage::Job { attempt_id, .. } = outbound.try_recv().unwrap() else { panic!("expected job") };
            if closed {
                receiver.take();
            }
            if full {
                scheduler.on_miner_message(
                    "miner",
                    MinerMessage::StreamChunk { attempt_id: attempt_id.clone(), seq: 0, chunk: json!({"choices": []}) },
                );
            }
            let terminal = if stream {
                MinerMessage::StreamEnd { attempt_id, usage: Some(json!({"completion_tokens": 1})) }
            } else {
                MinerMessage::Result { attempt_id, body: json!({"choices": []}) }
            };
            scheduler.on_miner_message("miner", terminal);
            let delivered = !closed && !full && !include_usage;
            assert!(scheduler.jobs.is_empty());
            assert_eq!(scheduler.stats.snapshot(0, 0).answered_per_second, u64::from(delivered) as f64 / 60.0);
            if let Some(mut receiver) = receiver {
                let event = receiver.try_recv().unwrap();
                if delivered {
                    assert!(matches!(event, JobEvent::End | JobEvent::Result { .. }));
                } else {
                    assert!(matches!(event, JobEvent::Chunk(_)));
                }
                assert!(matches!(receiver.try_recv(), Err(mpsc::error::TryRecvError::Disconnected)));
            }
            scheduler.advance_stats(now + Duration::from_secs(120));
            assert_eq!(scheduler.stats.snapshot(0, 0).answered_last_24h, u64::from(delivered));
        }
    }
}
