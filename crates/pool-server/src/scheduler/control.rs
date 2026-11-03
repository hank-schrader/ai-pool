//! Single-owner scheduler event loop and miner registration.

use super::*;

impl Scheduler {
    pub(super) async fn run(mut self, mut commands: mpsc::UnboundedReceiver<Command>) {
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

    pub(super) fn publish(&mut self) {
        self.stats.observe_queue(self.tally.queued_questions);
        self.public_stats.send_replace(self.stats.snapshot(
            self.tally.queued_questions,
            self.connected_miners(),
            self.tally.answering_questions,
        ));
        self.admission.queue_len.store(self.queue.len(), Ordering::Relaxed);
        self.admission.queue_capacity.store(self.queue_capacities("").0, Ordering::Relaxed);
    }

    pub(super) fn connected_miners(&self) -> usize {
        self.miners.values().filter(|miner| !miner.draining).count()
    }

    pub(super) fn advance_stats(&mut self, now: Instant) {
        self.stats.advance(now, self.tally.queued_questions, self.connected_miners(), self.tally.answering_questions);
    }

    pub(super) fn handle(&mut self, command: Command) {
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
                        performance: HashMap::new(),
                        pending_counts: 0,
                    },
                );
            }
            Command::MinerMessage { session, message } => self.on_miner_message(&session, message),
            Command::MinerGone { session } => self.remove_miner(&session, "miner disconnected"),
            Command::Count { count_id, request, reply } => self.start_count(count_id, request, reply),
            Command::CancelCount { count_id } => self.cancel_count(&count_id),
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
}

#[cfg(test)]
mod tests {
    use super::super::snapshot::test_support::*;
    use super::super::*;
    #[test]
    fn questions_left_waiting_after_dispatch_keep_their_bucket_peak() {
        let mut scheduler = scheduler();
        let now = Instant::now();
        scheduler.stats = Stats::new(now, Duration::from_secs(120));
        let mut outbound = miner(&mut scheduler, "miner", "cuda-4096");
        scheduler.advance_stats(now + Duration::from_secs(60));
        let (events, _receiver) = mpsc::channel(2);
        scheduler.admit(job(&scheduler, "busy", 100, None), events.clone()).unwrap();
        scheduler.pump();
        scheduler.publish();
        let PoolMessage::Job { attempt_id, .. } = outbound.try_recv().unwrap() else { panic!("expected active job") };
        let mut waiting = job(&scheduler, "waiting", 100, None);
        waiting.request.question_count = 4;
        scheduler.admit(waiting, events).unwrap();
        scheduler.pump();
        scheduler.publish();
        assert_eq!(scheduler.public_stats.borrow().queued_questions, 4);
        // The occupied slot frees later in the same second; the next dispatch pass clears the queue.
        scheduler.advance_stats(now + Duration::from_millis(60_200));
        scheduler.drop_job("busy", None);
        scheduler.on_miner_message("miner", MinerMessage::Cancelled { attempt_id });
        scheduler.publish();
        assert_eq!(scheduler.public_stats.borrow().queued_questions, 0);
        assert_eq!(scheduler.public_stats.borrow().answering_questions, 4);
        scheduler.advance_stats(now + Duration::from_secs(61));
        scheduler.publish();
        assert_eq!(scheduler.public_stats.borrow().live.last().unwrap().queued, 4);
        scheduler.advance_stats(now + Duration::from_secs(62));
        scheduler.publish();
        assert_eq!(scheduler.public_stats.borrow().live.last().unwrap().queued, 0);
        scheduler.advance_stats(now + Duration::from_secs(120));
        scheduler.publish();
        assert_eq!(scheduler.public_stats.borrow().history.queued, [4]);
        scheduler.advance_stats(now + Duration::from_secs(180));
        scheduler.publish();
        assert_eq!(scheduler.public_stats.borrow().history.queued, [4, 0]);
    }

    #[cfg(debug_assertions)]
    #[test]
    fn tally_follows_a_job_and_drops_empty_entries() {
        let mut tally = Tally::default();
        tally.enqueue("c", "m", 3);
        tally.enqueue("c", "m", 3);
        assert_eq!(tally.queued_questions, 6);
        assert_eq!(tally.answering_questions, 0);
        assert_eq!(tally.client("c"), (0, 2));
        assert_eq!(tally.model_queued("m"), 2);
        tally.activate("c", "m", 3);
        assert_eq!(tally.queued_questions, 3);
        assert_eq!(tally.answering_questions, 3);
        assert_eq!((tally.client("c"), tally.model_queued("m")), ((1, 1), 1));
        tally.deactivate("c", "m", 3);
        assert_eq!(tally.queued_questions, 6);
        assert_eq!(tally.answering_questions, 0);
        assert_eq!((tally.client("c"), tally.model_queued("m")), ((0, 2), 2));
        tally.activate("c", "m", 3);
        tally.release_active("c", 3);
        tally.dequeue("c", "m", 3);
        assert_eq!(tally.queued_questions, 0);
        assert_eq!(tally.answering_questions, 0);
        assert!(tally.clients.is_empty() && tally.models.is_empty());
    }

    #[test]
    fn older_long_job_gets_large_slot_before_faster_short_job() {
        let mut scheduler = scheduler();
        let mut small = miner(&mut scheduler, "small", "cuda-4096");
        let mut large = miner(&mut scheduler, "large", "cuda-16384");
        speed(&mut scheduler, "small", 100.0, 1);
        speed(&mut scheduler, "large", 10000.0, 1);
        let _long = enqueue(&mut scheduler, "long", 5000);
        let _short = enqueue(&mut scheduler, "short", 100);
        scheduler.pump();
        assert!(matches!(large.try_recv().unwrap(), PoolMessage::Job { job_id, .. } if job_id == "long"));
        assert!(matches!(small.try_recv().unwrap(), PoolMessage::Job { job_id, .. } if job_id == "short"));
    }

    #[test]
    fn retry_after_follows_throughput_and_is_clamped() {
        assert_eq!(retry_after(1, 0), 60);
        assert_eq!(retry_after(1, 600), 1);
        assert_eq!(retry_after(1, 30), 2);
        assert_eq!(retry_after(10, 60), 10);
        assert_eq!(retry_after(500, 10), 60);
    }
}
