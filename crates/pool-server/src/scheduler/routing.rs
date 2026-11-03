//! Select a free compatible profile by predicted finish time, preserving legacy scheduling.

use super::*;

impl Scheduler {
    pub(super) fn speed_prior(&self) -> f64 {
        let mut rates: Vec<_> = self
            .miners
            .values()
            .filter(|miner| !miner.draining)
            .flat_map(|miner| {
                miner
                    .devices
                    .iter()
                    .filter_map(|device| miner.performance.get(&device.id).and_then(|p| p.tokens_per_second))
            })
            .collect();
        rates.sort_by(f64::total_cmp);
        match rates.len() {
            0 => DEFAULT_TOKENS_PER_SECOND,
            n if n % 2 == 0 => (rates[n / 2 - 1] + rates[n / 2]) / 2.0,
            n => rates[n / 2],
        }
    }

    pub(super) fn find_slot(&self, job: &Job, rotation: usize) -> Option<(String, String, String)> {
        let prior = self.speed_prior();
        let now = Instant::now();
        let mut candidates = Vec::new();
        for (session, miner) in &self.miners {
            if miner.draining || job.excluded.contains(session) {
                continue;
            }
            for loaded in miner.serves(&job.new.model, &job.new.model_revision) {
                let busy = miner.busy(&loaded.device);
                if loaded.context_tokens >= job.new.routing_context()
                    && busy < miner.slots(&loaded.device)
                    && runtime_matches(loaded, job.new.request.routing.runtime_fingerprint.as_deref())
                {
                    let performance = miner.performance.get(&loaded.device);
                    let rate = performance.map_or(prior, |p| p.rate(prior, now));
                    let outstanding = performance.map_or(0, |p| p.outstanding_tokens);
                    let finish = (outstanding + job.new.work_tokens()) as f64 / rate;
                    let probe_due = performance.is_some_and(|p| p.probe_due(now));
                    candidates.push((
                        !probe_due,
                        finish,
                        loaded.context_tokens,
                        busy,
                        session.clone(),
                        loaded.device.clone(),
                        loaded.profile.clone(),
                    ));
                }
            }
        }
        candidates.sort_by(|a, b| {
            a.0.cmp(&b.0)
                .then_with(|| a.1.total_cmp(&b.1))
                .then_with(|| (&a.2, &a.3, &a.4, &a.5, &a.6).cmp(&(&b.2, &b.3, &b.4, &b.5, &b.6)))
        });
        let best = candidates.first().map(|c| (c.0, c.1, c.2, c.3))?;
        let tied: Vec<_> = candidates.into_iter().filter(|c| (c.0, c.1, c.2, c.3) == best).collect();
        let (_, _, _, _, session, device, profile) = tied[rotation % tied.len()].clone();
        Some((session, device, profile))
    }
}

#[cfg(test)]
mod tests {
    use super::super::snapshot::test_support::*;
    use super::super::*;
    #[tokio::test(start_paused = true)]
    async fn known_slow_devices_get_at_most_one_probe_per_idle_half_life() {
        let mut scheduler = scheduler();
        let mut slow = miner(&mut scheduler, "slow", "cuda-16384");
        let _steady = miner(&mut scheduler, "steady", "cuda-4096");
        speed(&mut scheduler, "slow", 500.0, 2);
        speed(&mut scheduler, "steady", 6000.0, 1);
        let now = Instant::now();
        let performance = &scheduler.miners["slow"].performance["gpu0"];
        assert_eq!(performance.rate(6000.0, now + SPEED_AGE_THRESHOLD), 500.0);
        assert_eq!(performance.rate(6000.0, now + SPEED_AGE_THRESHOLD + SPEED_HALF_LIFE), 3250.0);
        let _probe = enqueue(&mut scheduler, "probe", 1000);
        tokio::time::advance(SPEED_HALF_LIFE - Duration::from_secs(1)).await;
        // The steady device is serving the low-concurrency traffic, so its
        // successful observations remain fresh while the slow device is idle.
        speed(&mut scheduler, "steady", 6000.0, 1);
        assert_eq!(selected(&scheduler, "probe", 0), "steady");
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(selected(&scheduler, "probe", 0), "slow");
        scheduler.pump();
        let PoolMessage::Job { attempt_id, .. } = slow.try_recv().unwrap() else { panic!("expected probe") };
        // A second free slot does not allow a second probe while the first runs.
        let _other = enqueue(&mut scheduler, "other", 1000);
        assert_eq!(selected(&scheduler, "other", 0), "steady");
        scheduler.drop_job("other", None);
        tokio::time::advance(Duration::from_secs(2)).await;
        scheduler.on_miner_message("slow", MinerMessage::Result { attempt_id, body: json!({}), runtime: None });
        assert_eq!(scheduler.miners["slow"].performance["gpu0"].tokens_per_second, Some(500.0));
        let finished_at = Instant::now();
        for seconds in [0, 60, 120, 240, 299, 300] {
            tokio::time::advance(
                (finished_at + Duration::from_secs(seconds)).saturating_duration_since(Instant::now()),
            )
            .await;
            speed(&mut scheduler, "steady", 6000.0, 1);
            let id = format!("later{seconds}");
            let _events = enqueue(&mut scheduler, &id, 1000);
            assert_eq!(selected(&scheduler, &id, 0), if seconds < 300 { "steady" } else { "slow" });
            scheduler.drop_job(&id, None);
        }
        let _failed = enqueue(&mut scheduler, "failed_probe", 1000);
        scheduler.pump();
        let PoolMessage::Job { attempt_id, .. } = slow.try_recv().unwrap() else { panic!("expected second probe") };
        tokio::time::advance(Duration::from_secs(1)).await;
        scheduler.on_miner_message(
            "slow",
            MinerMessage::JobError { attempt_id, code: JobErrorCode::InvalidRequest, message: "failed probe".into() },
        );
        let performance = &scheduler.miners["slow"].performance["gpu0"];
        assert_eq!(performance.tokens_per_second, Some(500.0));
        assert!(!performance.probe_due(Instant::now() + SPEED_HALF_LIFE - Duration::from_secs(1)));
        assert!(performance.probe_due(Instant::now() + SPEED_HALF_LIFE));
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
    fn staggered_completion_burst_retains_its_execution_time() {
        let mut performance = DevicePerformance::default();
        let now = Instant::now();
        let attempts: Vec<_> = (0..128)
            .map(|i| {
                performance.start(1000, now);
                Attempt { job: Some(i.to_string()), device: "gpu0".into(), work_tokens: 1000 }
            })
            .collect();
        for (i, attempt) in attempts.iter().enumerate() {
            performance.finish(attempt, true, now + Duration::from_secs(30) + Duration::from_micros(i as u64));
        }
        let expected = 128000.0 / (30.0 + 0.000127);
        assert!((performance.tokens_per_second.unwrap() - expected).abs() < 0.001);
        assert_eq!(performance.outstanding_tokens, 0);
        assert_eq!(performance.samples.len(), 2);
    }
}
