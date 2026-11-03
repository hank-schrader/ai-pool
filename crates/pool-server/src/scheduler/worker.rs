//! Per-device slot accounting and observed throughput, excluding failed work and idle time.

use super::*;

pub(super) struct Miner {
    pub(super) label: String,
    pub(super) miner_id: String,
    pub(super) hardware: Hardware,
    pub(super) outbound: mpsc::Sender<PoolMessage>,
    pub(super) devices: Vec<DeviceSlots>,
    pub(super) models: Vec<LoadedModel>,
    pub(super) draining: bool,
    pub(super) last_seen: Instant,
    /// Attempts holding a device slot. `job` is None once the job is gone
    /// but the miner has not yet confirmed the work stopped.
    pub(super) attempts: HashMap<String, Attempt>,
    pub(super) performance: HashMap<String, DevicePerformance>,
    pub(super) pending_counts: usize,
}

pub(super) struct Attempt {
    pub(super) job: Option<String>,
    pub(super) device: String,
    pub(super) work_tokens: u64,
}

/// Completed work divided by busy wall time, never an average of per-job rates.
/// Observations cover the latest 60 busy seconds, retaining the whole bucket
/// crossing the boundary. Subsecond completion intervals share a bucket so burst
/// delivery cannot evict execution time. Idle and failed-only time is excluded.
#[derive(Default)]
pub(super) struct DevicePerformance {
    pub(super) tokens_per_second: Option<f64>,
    pub(super) outstanding_tokens: u64,
    pub(super) active: u32,
    pub(super) updated_at: Option<Instant>,
    pub(super) idle_since: Option<Instant>,
    pub(super) last_sample: Option<Instant>,
    pub(super) pending_busy_seconds: f64,
    pub(super) samples: VecDeque<(u64, f64)>,
}

impl DevicePerformance {
    pub(super) fn advance(&mut self, now: Instant) {
        if let Some(last) = self.updated_at
            && self.active > 0
        {
            self.pending_busy_seconds += now.saturating_duration_since(last).as_secs_f64();
        }
        self.updated_at = Some(now);
    }

    pub(super) fn probe_due(&self, now: Instant) -> bool {
        self.active == 0
            && self.last_sample.is_some_and(|at| now.saturating_duration_since(at) >= SPEED_HALF_LIFE)
            && self.idle_since.is_some_and(|at| now.saturating_duration_since(at) >= SPEED_HALF_LIFE)
    }

    pub(super) fn rate(&self, prior: f64, now: Instant) -> f64 {
        let Some(measured) = self.tokens_per_second else { return prior };
        if self.active > 0 {
            return measured;
        }
        let age = self.last_sample.map_or(Duration::ZERO, |at| now.saturating_duration_since(at));
        let decay_seconds = age.saturating_sub(SPEED_AGE_THRESHOLD).as_secs_f64();
        let weight = 2.0_f64.powf(-decay_seconds / SPEED_HALF_LIFE.as_secs_f64());
        (weight * measured + (1.0 - weight) * prior).clamp(MIN_TOKENS_PER_SECOND, MAX_TOKENS_PER_SECOND)
    }

    pub(super) fn start(&mut self, work_tokens: u64, now: Instant) {
        // A recovery probe must replace a stale observation, rather than blend
        // it with an old warmup/outlier that caused the starvation.
        if self.probe_due(now) {
            self.samples.clear();
        }
        self.advance(now);
        self.active += 1;
        self.outstanding_tokens += work_tokens;
        self.idle_since = None;
    }

    pub(super) fn finish(&mut self, attempt: &Attempt, successful: bool, now: Instant) {
        self.advance(now);
        self.active -= 1;
        self.outstanding_tokens -= attempt.work_tokens;
        if successful {
            let seconds = std::mem::take(&mut self.pending_busy_seconds);
            // Accumulate short intervals into roughly one-busy-second buckets.
            // Long intervals remain whole: a delayed burst's execution time
            // must survive until 60 later busy seconds have actually elapsed.
            if let Some(last) = self.samples.back_mut()
                && (last.1 < 1.0 || seconds == 0.0)
            {
                last.0 += attempt.work_tokens;
                last.1 += seconds;
            } else {
                self.samples.push_back((attempt.work_tokens, seconds));
            }
            let mut seconds: f64 = self.samples.iter().map(|s| s.1).sum();
            while self.samples.front().is_some_and(|first| seconds - first.1 >= SPEED_WINDOW.as_secs_f64()) {
                seconds -= self.samples.pop_front().expect("checked above").1;
            }
            let tokens: u64 = self.samples.iter().map(|s| s.0).sum();
            let seconds = seconds.max(0.000_001);
            self.tokens_per_second =
                Some((tokens as f64 / seconds).clamp(MIN_TOKENS_PER_SECOND, MAX_TOKENS_PER_SECOND));
            self.last_sample = Some(now);
        }
        if self.active == 0 {
            self.idle_since = Some(now);
            self.pending_busy_seconds = 0.0;
        }
    }
}

impl Miner {
    pub(super) fn busy(&self, device: &str) -> u32 {
        self.attempts.values().filter(|attempt| attempt.device == device).count() as u32
    }

    pub(super) fn slots(&self, device: &str) -> u32 {
        self.devices.iter().find(|d| d.id == device).map_or(0, |d| d.slots)
    }

    pub(super) fn serves(&self, model: &str, revision: &str) -> impl Iterator<Item = &LoadedModel> {
        self.models.iter().filter(move |m| m.model == model && m.model_revision == revision)
    }
}

#[cfg(test)]
mod tests {
    use super::super::snapshot::test_support::*;
    use super::super::*;
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
            scheduler.debug_check();
            assert_eq!(scheduler.tally.answering_questions, 0);
            assert_eq!(scheduler.stats.snapshot(0, 0, 0).average_prompt_tokens, None);
            scheduler.miners.clear();
            let (events, _receiver) = mpsc::channel(1);
            assert_eq!(
                scheduler.admit(job(&scheduler, "empty", 100, minimum), events).unwrap_err().code,
                "no_miner_available"
            );
        }
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
}
