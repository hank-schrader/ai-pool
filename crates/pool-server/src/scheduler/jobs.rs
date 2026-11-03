//! Release slots only after work stops; cancel dropped jobs and handle miner loss.

use super::*;

impl Scheduler {
    pub(super) fn job_for(&mut self, session: &str, attempt_id: &str) -> Option<&mut Job> {
        let job_id = self.miners.get(session)?.attempts.get(attempt_id)?.job.clone()?;
        let job = self.jobs.get_mut(&job_id)?;
        matches!(&job.state, JobState::Dispatched { attempt, .. } if attempt == attempt_id).then_some(job)
    }

    pub(super) fn release(&mut self, session: &str, attempt_id: &str, successful: bool) -> Option<String> {
        let miner = self.miners.get_mut(session)?;
        let attempt = miner.attempts.remove(attempt_id)?;
        miner.performance.get_mut(&attempt.device).expect("dispatched device has performance state").finish(
            &attempt,
            successful && attempt.job.is_some(),
            Instant::now(),
        );
        let job_id = attempt.job?;
        let job = self.jobs.get(&job_id)?;
        matches!(&job.state, JobState::Dispatched { attempt, .. } if attempt == attempt_id).then_some(job_id)
    }

    pub(super) fn fail(&mut self, job_id: &str, error: ApiError) {
        self.drop_job(job_id, Some(error));
    }

    pub(super) fn remove_job(&mut self, job_id: &str) -> Option<Job> {
        let job = self.jobs.remove(job_id)?;
        self.tally.forget(&job);
        self.admission.record_completion(&job.new.client);
        Some(job)
    }

    pub(super) fn drop_job(&mut self, job_id: &str, error: Option<ApiError>) {
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
            let _ = job.events.try_send(JobEvent::Failed(error.with_attempts(job.attempts).with_request_id(job_id)));
        }
    }

    pub(super) fn remove_miner(&mut self, session: &str, reason: &str) {
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
            self.send_count(count_id, pending.request, pending.reply, pending.deadline, &excluded);
        }
        self.pump();
    }
}

#[cfg(test)]
mod tests {
    use super::super::snapshot::test_support::*;
    use super::super::*;
    #[tokio::test(start_paused = true)]
    async fn idle_probe_recovers_an_underestimated_device_with_sequential_traffic() {
        let mut scheduler = scheduler();
        let mut recovering = miner(&mut scheduler, "recovering", "cuda-16384");
        let _warmup = enqueue(&mut scheduler, "warmup", 1000);
        scheduler.pump();
        let PoolMessage::Job { attempt_id, .. } = recovering.try_recv().unwrap() else { panic!("expected warmup") };
        tokio::time::advance(Duration::from_secs(2)).await;
        scheduler.on_miner_message("recovering", MinerMessage::Result { attempt_id, body: json!({}), runtime: None });
        assert_eq!(scheduler.miners["recovering"].performance["gpu0"].tokens_per_second, Some(500.0));
        let mut steady = miner(&mut scheduler, "steady", "cuda-4096");
        speed(&mut scheduler, "steady", 6000.0, 1);
        let idle_at = Instant::now();
        // Only one job in flight: outstanding work never forces exploration.
        let mut receivers = Vec::new();
        for seconds in [0, 60, 120, 180, 240] {
            tokio::time::advance((idle_at + Duration::from_secs(seconds)).saturating_duration_since(Instant::now()))
                .await;
            let id = format!("sequential{seconds}");
            receivers.push(enqueue(&mut scheduler, &id, 1000));
            assert_eq!(selected(&scheduler, &id, 0), "steady");
            scheduler.pump();
            let PoolMessage::Job { attempt_id, .. } = steady.try_recv().unwrap() else { panic!("expected steady job") };
            tokio::time::advance(Duration::from_millis(167)).await;
            scheduler.on_miner_message("steady", MinerMessage::Result { attempt_id, body: json!({}), runtime: None });
        }
        tokio::time::advance((idle_at + SPEED_HALF_LIFE).saturating_duration_since(Instant::now())).await;
        let _probe = enqueue(&mut scheduler, "probe", 1000);
        assert_eq!(selected(&scheduler, "probe", 0), "recovering");
        scheduler.pump();
        let PoolMessage::Job { attempt_id, .. } = recovering.try_recv().unwrap() else { panic!("expected probe") };
        tokio::time::advance(Duration::from_millis(100)).await;
        scheduler.on_miner_message("recovering", MinerMessage::Result { attempt_id, body: json!({}), runtime: None });
        assert!((scheduler.miners["recovering"].performance["gpu0"].tokens_per_second.unwrap() - 10000.0).abs() < 0.01);
        let _next = enqueue(&mut scheduler, "next", 1000);
        assert_eq!(selected(&scheduler, "next", 0), "recovering");
    }

    #[test]
    fn equal_finish_prefers_small_context_then_fewer_busy_slots_then_rotation() {
        let mut scheduler = scheduler();
        let _small = miner(&mut scheduler, "small", "cuda-4096");
        let _a = miner(&mut scheduler, "a", "cuda-16384");
        let _b = miner(&mut scheduler, "b", "cuda-16384");
        speed(&mut scheduler, "a", 1000.0, 1);
        speed(&mut scheduler, "b", 1000.0, 1);
        let _first = enqueue(&mut scheduler, "first", 100);
        assert_eq!(selected(&scheduler, "first", 0), "small");
        scheduler.pump();
        let _next = enqueue(&mut scheduler, "next", 100);
        assert_eq!(selected(&scheduler, "next", 0), "a");
        assert_eq!(selected(&scheduler, "next", 1), "b");
        // Equal finish despite one device having more outstanding jobs.
        speed(&mut scheduler, "small", 2000.0, 2);
        assert_eq!(selected(&scheduler, "next", 0), "small"); // context breaks tie first
        scheduler.miners.get_mut("small").unwrap().models[0].context_tokens = 16384;
        assert_eq!(selected(&scheduler, "next", 0), "a"); // fewer busy slots
        scheduler.jobs.get_mut("next").unwrap().excluded.insert("a".into());
        assert_eq!(selected(&scheduler, "next", 0), "b");
        scheduler.miners.get_mut("b").unwrap().draining = true;
        assert_eq!(selected(&scheduler, "next", 0), "small");
    }

    #[test]
    fn concurrent_completions_measure_aggregate_capacity_and_ignore_other_devices() {
        let mut a = DevicePerformance::default();
        let mut b = DevicePerformance::default();
        let now = Instant::now();
        a.start(1000, now);
        a.start(1000, now);
        b.start(100, now);
        let first = Attempt { job: Some("a".into()), device: "gpu0".into(), work_tokens: 1000 };
        let second = Attempt { job: Some("b".into()), device: "gpu0".into(), work_tokens: 1000 };
        let other = Attempt { job: Some("c".into()), device: "gpu1".into(), work_tokens: 100 };
        a.finish(&first, true, now + Duration::from_secs(2));
        a.finish(&second, true, now + Duration::from_secs(2));
        b.finish(&other, true, now + Duration::from_secs(2));
        assert_eq!(a.tokens_per_second, Some(1000.0));
        assert_eq!(b.tokens_per_second, Some(50.0));
        assert_eq!((a.active, a.outstanding_tokens), (0, 0));
    }
}
