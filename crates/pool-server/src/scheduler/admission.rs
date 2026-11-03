//! Shared admission reservations; dropping a request releases its place in the pipeline.

use super::*;

/// Admission state shared between the HTTP handlers and the scheduler, so a
/// request can be refused before its body is read, parsed or counted.
pub struct Admission {
    /// `max_active + max_queued` per client; `None` means no per-client limit.
    pub(super) per_client: Option<usize>,
    /// Requests per client anywhere in the pipeline (parsing, counting, queued, running).
    pub(super) inflight: Mutex<HashMap<String, usize>>,
    /// Published by the scheduler after every command.
    pub(super) queue_len: AtomicUsize,
    pub(super) queue_capacity: AtomicUsize,
    /// Recent job completions (any outcome), for Retry-After.
    pub(super) completions: Mutex<VecDeque<(Instant, String)>>,
}

/// One client's slot in the pipeline; released when dropped, on every exit path.
pub struct Reservation {
    pub(super) admission: Arc<Admission>,
    pub(super) client: Option<String>,
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

    pub(super) fn record_completion(&self, client: &str) {
        let now = Instant::now();
        let mut completions = lock(&self.completions);
        completions.push_back((now, client.into()));
        Self::prune(&mut completions, now);
    }

    /// Completions within [`RATE_WINDOW`], by one client or by everyone.
    pub(super) fn completed(&self, client: Option<&str>) -> usize {
        let mut completions = lock(&self.completions);
        Self::prune(&mut completions, Instant::now());
        completions.iter().filter(|(_, who)| client.is_none_or(|c| c == who)).count()
    }

    pub(super) fn prune(completions: &mut VecDeque<(Instant, String)>, now: Instant) {
        while completions.front().is_some_and(|(at, _)| now.duration_since(*at) > RATE_WINDOW) {
            completions.pop_front();
        }
    }
}

/// Seconds until `ahead` jobs have probably finished, from the completions seen in
/// the last [`RATE_WINDOW`]: ceil(ahead / (done / window)), clamped to [1, 60].
/// Nothing completed recently means no usable rate, so the maximum.
pub(super) fn retry_after(ahead: usize, done: usize) -> u64 {
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

#[cfg(test)]
mod tests {
    use super::super::snapshot::test_support::*;
    use super::super::*;
    #[test]
    fn unknown_devices_use_median_prior_and_outstanding_work_prevents_hogging() {
        let mut scheduler = scheduler();
        let _a = miner(&mut scheduler, "a", "cuda-16384");
        let _b = miner(&mut scheduler, "b", "cuda-16384");
        let _new = miner(&mut scheduler, "new", "cuda-4096");
        assert_eq!(scheduler.speed_prior(), DEFAULT_TOKENS_PER_SECOND);
        speed(&mut scheduler, "a", 2000.0, 12);
        assert_eq!(scheduler.speed_prior(), 2000.0);
        speed(&mut scheduler, "b", 4000.0, 12);
        scheduler.miners.get_mut("new").unwrap().devices[0].slots = 12;
        assert_eq!(scheduler.speed_prior(), 3000.0);
        let _first = enqueue(&mut scheduler, "first", 100);
        assert_eq!(selected(&scheduler, "first", 0), "b");
        scheduler.pump();
        let _next = enqueue(&mut scheduler, "next", 100);
        assert_eq!(selected(&scheduler, "next", 0), "new");
        scheduler.pump();
        let _third = enqueue(&mut scheduler, "third", 100);
        assert_eq!(selected(&scheduler, "third", 0), "a");
        // Repeated capacity announcements do not reset a learned rate.
        let devices = scheduler.miners["a"].devices.clone();
        let models = scheduler.miners["a"].models.clone();
        scheduler.on_capacity("a", devices, models);
        assert_eq!(scheduler.miners["a"].performance["gpu0"].tokens_per_second, Some(2000.0));
        scheduler.remove_miner("a", "test reconnect");
        let _reconnected = miner(&mut scheduler, "reconnected", "cuda-16384");
        assert!(scheduler.miners["reconnected"].performance.values().all(|p| p.tokens_per_second.is_none()));
        assert_eq!(scheduler.speed_prior(), 4000.0);
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
}
