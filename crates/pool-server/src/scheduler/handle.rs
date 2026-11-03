//! Typed scheduler commands and HTTP guards; cancellation follows request lifetime.

use super::*;

pub enum JobEvent {
    /// Complete non-streaming response.
    Result {
        body: Value,
        profile: String,
        runtime: Option<Box<pool_protocol::runtime::RuntimeDescriptor>>,
        attempts: u32,
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
    pub(super) fn routing_context(&self) -> u32 {
        self.required_context.max(self.request.min_context_tokens.unwrap_or(0))
    }

    /// Every question runs a full forward; the context count is only the longest
    /// prompt. Keep that count unchanged for context checks and public stats.
    pub(super) fn work_tokens(&self) -> u64 {
        (u64::from(self.input_tokens) * self.request.question_count).max(1)
    }
}

pub struct CountRequest {
    pub model: String,
    pub model_revision: String,
    pub operation: Operation,
    pub payload: Value,
    pub runtime_fingerprint: Option<String>,
}

pub struct CountReply {
    pub input_tokens: u32,
    pub loaded: LoadedModel,
}

pub(super) enum Command {
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
    CancelCount {
        count_id: String,
    },
    Count {
        count_id: String,
        request: CountRequest,
        reply: oneshot::Sender<Result<CountReply, ApiError>>,
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
    pub(super) commands: mpsc::UnboundedSender<Command>,
    pub(super) admission: Arc<Admission>,
    pub(super) stats: watch::Receiver<StatsSnapshot>,
}

impl SchedulerHandle {
    pub(super) fn send(&self, command: Command) {
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

    pub async fn count(&self, request: CountRequest) -> Result<CountReply, ApiError> {
        let count_id = format!("cnt_{}", uuid::Uuid::new_v4().simple());
        let _guard = CountGuard { handle: self.clone(), count_id: count_id.clone() };
        let (reply, response) = oneshot::channel();
        self.send(Command::Count { count_id, request, reply });
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

    /// Independent network readers never enqueue scheduler commands.
    pub fn stats_receiver(&self) -> watch::Receiver<StatsSnapshot> {
        self.stats.clone()
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
    pub(super) handle: SchedulerHandle,
    pub(super) job_id: String,
    pub(super) finished: bool,
    pub(super) _reservation: Reservation,
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
