//! Enforce count, job, heartbeat and dispatch-ack deadlines.

use super::*;

impl Scheduler {
    pub(super) fn expire(&mut self, now: Instant) {
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

        let expired_counts: Vec<String> = self
            .counts
            .iter()
            .filter(|(_, count)| now >= count.deadline || count.reply.is_closed())
            .map(|(id, _)| id.clone())
            .collect();
        for count_id in expired_counts {
            let error = ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "count_timeout", "token counting timed out");
            self.finish_count(&count_id, Err(error.retry_after(2)));
            self.cancel_count(&count_id);
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

    pub(super) fn debug_check(&self) {
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
            debug_assert_eq!(
                self.tally.answering_questions,
                self.jobs
                    .values()
                    .filter(|job| matches!(job.state, JobState::Dispatched { .. }))
                    .map(|job| job.new.request.question_count)
                    .sum::<u64>(),
                "answering question counter drifted"
            );
            debug_assert_eq!(active + queued, self.jobs.len(), "job counter drifted");
            debug_assert_eq!(self.tally.models.values().sum::<usize>(), queued, "model counter drifted");
            debug_assert!(self.tally.clients.values().all(|c| *c != (0, 0)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::snapshot::test_support::*;
    use super::super::*;
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
        assert_eq!(scheduler.stats.snapshot(0, 0, 0).answered_per_second, 0.0);
        let PoolMessage::Job { attempt_id, .. } = second.try_recv().unwrap() else { panic!("expected retry") };
        let result = MinerMessage::Result { attempt_id, body: json!({"answers": {}}), runtime: None };
        scheduler.on_miner_message("second", result.clone());
        scheduler.on_miner_message("second", result);
        assert_eq!(scheduler.stats.snapshot(0, 0, 0).answered_per_second, 3.0 / 60.0);
        scheduler.advance_stats(now + Duration::from_secs(120));
        let snapshot = scheduler.stats.snapshot(0, 0, 0);
        assert_eq!(snapshot.answered_last_24h, 3);
        assert_eq!(snapshot.history.answered, [3]);
        assert_eq!(snapshot.history.requests, [1]);
        assert_eq!(snapshot.history.prompt_tokens, [100]);
    }

    #[test]
    fn two_prefetch_waves_measure_work_over_busy_wall_time() {
        let mut performance = DevicePerformance::default();
        let now = Instant::now();
        let attempts: Vec<_> = (0..12)
            .map(|i| {
                performance.start(1000, now);
                Attempt { job: Some(i.to_string()), device: "gpu0".into(), work_tokens: 1000 }
            })
            .collect();
        for attempt in &attempts[..6] {
            performance.finish(attempt, true, now + Duration::from_secs(1));
        }
        assert_eq!(performance.tokens_per_second, Some(6000.0));
        for attempt in &attempts[6..] {
            performance.finish(attempt, true, now + Duration::from_secs(2));
        }
        assert_eq!(performance.tokens_per_second, Some(6000.0));
        assert_eq!(performance.outstanding_tokens, 0);
    }

    #[test]
    fn dropping_every_reservation_empties_the_table() {
        let admission = Admission::new(Some(1));
        drop(admission.reserve("a").unwrap());
        assert!(admission.inflight.lock().unwrap().is_empty());
    }
}
