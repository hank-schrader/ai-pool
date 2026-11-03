//! Incremental admission and question totals; no full job-table scan in the hot path.

use super::*;

/// Incremental job counts, so admission and dispatch never scan every job.
/// `clients` maps a client to (dispatched, queued) jobs, `models` maps a model
/// to its queued jobs; entries are removed when they reach zero.
#[derive(Default)]
pub(super) struct Tally {
    pub(super) clients: HashMap<String, (usize, usize)>,
    pub(super) models: HashMap<String, usize>,
    pub(super) queued_questions: u64,
    pub(super) answering_questions: u64,
}

impl Tally {
    pub(super) fn client(&self, client: &str) -> (usize, usize) {
        self.clients.get(client).copied().unwrap_or((0, 0))
    }

    pub(super) fn model_queued(&self, model: &str) -> usize {
        self.models.get(model).copied().unwrap_or(0)
    }

    /// A job entered the queue (admitted, or sent back to it).
    pub(super) fn enqueue(&mut self, client: &str, model: &str, questions: u64) {
        self.queued_questions += questions;
        self.clients.entry(client.to_owned()).or_default().1 += 1;
        *self.models.entry(model.to_owned()).or_default() += 1;
    }

    /// A queued job left the queue without being tracked any further.
    pub(super) fn dequeue(&mut self, client: &str, model: &str, questions: u64) {
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
    pub(super) fn activate(&mut self, client: &str, model: &str, questions: u64) {
        self.dequeue(client, model, questions);
        self.answering_questions += questions;
        self.clients.entry(client.to_owned()).or_default().0 += 1;
    }

    /// A dispatched job went back to the queue.
    pub(super) fn deactivate(&mut self, client: &str, model: &str, questions: u64) {
        self.release_active(client, questions);
        self.enqueue(client, model, questions);
    }

    pub(super) fn release_active(&mut self, client: &str, questions: u64) {
        self.answering_questions -= questions;
        let entry = self.clients.get_mut(client).expect("client has an active job");
        entry.0 -= 1;
        if *entry == (0, 0) {
            self.clients.remove(client);
        }
    }

    /// A job is being removed from the scheduler in its current state.
    pub(super) fn forget(&mut self, job: &Job) {
        match job.state {
            JobState::Queued => self.dequeue(&job.new.client, &job.new.model, job.new.request.question_count),
            JobState::Dispatched { .. } => self.release_active(&job.new.client, job.new.request.question_count),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::snapshot::test_support::*;
    use super::super::*;
    #[tokio::test(start_paused = true)]
    async fn failed_rejected_cancelled_and_late_success_attempts_do_not_train_speed() {
        for outcome in 0..5 {
            let mut scheduler = scheduler();
            let mut outbound = miner(&mut scheduler, "miner", "cuda-4096");
            speed(&mut scheduler, "miner", 500.0, 2);
            let _events = enqueue(&mut scheduler, "job", 1000);
            scheduler.pump();
            let PoolMessage::Job { attempt_id, .. } = outbound.try_recv().unwrap() else { panic!("expected job") };
            tokio::time::advance(Duration::from_secs(1)).await;
            let terminal = match outcome {
                0 => MinerMessage::JobError { attempt_id, code: JobErrorCode::InvalidRequest, message: "bad".into() },
                1 => MinerMessage::Rejected { attempt_id, reason: RejectReason::Busy, message: "busy".into() },
                _ => {
                    scheduler.drop_job("job", None);
                    assert_eq!(scheduler.miners["miner"].performance["gpu0"].outstanding_tokens, 1000);
                    assert_eq!(scheduler.miners["miner"].busy("gpu0"), 1);
                    match outcome {
                        2 => MinerMessage::Cancelled { attempt_id },
                        3 => MinerMessage::Result { attempt_id, body: json!({}), runtime: None },
                        _ => MinerMessage::StreamEnd { attempt_id, usage: None },
                    }
                }
            };
            scheduler.on_miner_message("miner", terminal);
            let performance = &scheduler.miners["miner"].performance["gpu0"];
            assert_eq!(performance.tokens_per_second, Some(500.0));
            assert_eq!(performance.outstanding_tokens, 0);
            assert_eq!(performance.active, 0);
            // Failed-only busy time and the following idle gap must not leak
            // into a later successful observation's denominator.
            tokio::time::advance(Duration::from_secs(30)).await;
            let _later = enqueue(&mut scheduler, "later", 1000);
            scheduler.pump();
            let attempt_id = loop {
                if let PoolMessage::Job { attempt_id, job_id, .. } = outbound.try_recv().unwrap()
                    && job_id == "later"
                {
                    break attempt_id;
                }
            };
            tokio::time::advance(Duration::from_secs(1)).await;
            scheduler.on_miner_message("miner", MinerMessage::Result { attempt_id, body: json!({}), runtime: None });
            assert_eq!(scheduler.miners["miner"].performance["gpu0"].tokens_per_second, Some(1000.0));
        }
    }

    #[test]
    fn faster_large_device_wins_at_low_load_then_slow_device_helps_under_load() {
        let mut scheduler = scheduler();
        let mut slow = miner(&mut scheduler, "slow", "cuda-4096");
        let mut fast = miner(&mut scheduler, "fast", "cuda-16384");
        speed(&mut scheduler, "slow", 600.0, 2);
        speed(&mut scheduler, "fast", 4500.0, 12);
        scheduler.config.max_active_per_client = None;
        let _events = enqueue(&mut scheduler, "first", 1700);
        scheduler.pump();
        assert!(slow.try_recv().is_err());
        assert!(matches!(fast.try_recv().unwrap(), PoolMessage::Job { job_id, .. } if job_id == "first"));
        // Seven jobs still predict <= 2.65s on fast, versus 2.83s on slow.
        let _events: Vec<_> = (2..=8).map(|i| enqueue(&mut scheduler, &format!("job{i}"), 1700)).collect();
        scheduler.pump();
        assert_eq!(scheduler.miners["fast"].busy("gpu0"), 7);
        assert_eq!(scheduler.miners["slow"].busy("gpu0"), 1);
        assert!(matches!(slow.try_recv().unwrap(), PoolMessage::Job { .. }));
        let snapshot = scheduler.snapshot();
        let device = &snapshot.miners.iter().find(|m| m.session_id == "fast").unwrap().devices[0];
        assert_eq!(device.tokens_per_second, 4500.0);
        assert_eq!(device.outstanding_tokens, 7 * 1700);
    }

    #[test]
    fn speed_samples_are_bounded_and_zero_token_jobs_have_minimal_work() {
        for (tokens, duration, expected) in [
            (u64::from(u32::MAX), Duration::ZERO, Some(MAX_TOKENS_PER_SECOND)),
            (1, Duration::from_secs(10), Some(MIN_TOKENS_PER_SECOND)),
        ] {
            let mut performance = DevicePerformance::default();
            let now = Instant::now();
            performance.start(tokens, now);
            let attempt = Attempt { job: Some("j".into()), device: "gpu0".into(), work_tokens: tokens };
            performance.finish(&attempt, true, now + duration);
            assert_eq!(performance.tokens_per_second, expected);
        }
        let scheduler = scheduler();
        assert_eq!(job(&scheduler, "zero", 0, None).work_tokens(), 1);
    }
}
