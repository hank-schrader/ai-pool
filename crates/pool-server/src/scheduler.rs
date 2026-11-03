//! The scheduler: a single task that owns miners, device slots, the job queue
//! and pending token counts. HTTP handlers and miner connections talk to it
//! through commands, so slot reservation, cancellation and retries never race.
//!
//! Guarantees: a client gets at most one response. An attempt is retried once,
//! on another miner, only before anything was committed to the client. A slot
//! stays reserved until the miner confirms the work stopped or disconnects.

mod counts;
mod runtime;
use counts::CountGuard;
use runtime::{revision_mismatch, runtime_matches, unavailable, valid_runtime};

mod admission;
mod control;
mod dispatch;
mod expiry;
mod handle;
mod jobs;
mod messages;
mod retry;
mod routing;
mod shaping;
mod snapshot;
mod tally;
mod worker;
#[cfg(test)]
use admission::retry_after;
pub use admission::{Admission, Reservation};
use handle::Command;
pub use handle::{CountReply, CountRequest, JobEvent, JobGuard, NewJob, SchedulerHandle};
use shaping::{shape_chunk, shape_response};
use tally::Tally;
#[cfg(test)]
use worker::DevicePerformance;
use worker::{Attempt, Miner};

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
/// Conservative cold-start prior and bounds for noisy/near-instant completions.
const DEFAULT_TOKENS_PER_SECOND: f64 = 1_000.0;
const MIN_TOKENS_PER_SECOND: f64 = 1.0;
const MAX_TOKENS_PER_SECOND: f64 = 1_000_000.0;
const SPEED_WINDOW: Duration = Duration::from_secs(60);
const SPEED_AGE_THRESHOLD: Duration = Duration::from_secs(60);
const SPEED_HALF_LIFE: Duration = Duration::from_secs(300);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
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
    pub available_runtimes: Vec<LoadedModel>,
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
    pub tokens_per_second: f64,
    pub outstanding_tokens: u64,
}

enum JobState {
    Queued,
    Dispatched {
        session: String,
        attempt: String,
        profile: String,
        runtime: Option<Box<pool_protocol::runtime::RuntimeDescriptor>>,
        sent_at: Instant,
        accepted: bool,
    },
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
    loaded: LoadedModel,
    reply: oneshot::Sender<Result<CountReply, ApiError>>,
    session: String,
    deadline: Instant,
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
    let (public_stats, stats_receiver) = watch::channel(stats.snapshot(0, 0, 0));
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
