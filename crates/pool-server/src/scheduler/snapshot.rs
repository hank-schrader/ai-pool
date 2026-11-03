//! Publish catalog versus live availability; existing check helpers are kept under cfg(test).

use super::*;

impl Scheduler {
    pub(super) fn snapshot(&self) -> Snapshot {
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
                if loaded.runtime.is_some() {
                    entry.available_runtimes.push(loaded.clone());
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
        let prior = self.speed_prior();
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
                    .map(|d| {
                        let performance = miner.performance.get(&d.id);
                        DeviceStatus {
                            id: d.id.clone(),
                            slots: d.slots,
                            busy: miner.busy(&d.id),
                            tokens_per_second: performance.map_or(prior, |p| p.rate(prior, now)),
                            outstanding_tokens: performance.map_or(0, |p| p.outstanding_tokens),
                        }
                    })
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

#[cfg(test)]
pub(super) mod test_support {
    use super::super::*;
    use pool_protocol::{Accelerator, requests};
    pub(crate) fn config() -> Config {
        Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            catalog: "config/models.json".into(),
            auth_mode: crate::config::AuthMode::Local,
            miner_tokens: vec![],
            miner_auth: crate::config::MinerAuth::Token,
            max_miners: 256,
            max_stats_streams: 128,
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

    pub(crate) fn scheduler() -> Scheduler {
        let mut config = config();
        config.queue_capacity_global = None;
        config.queue_capacity_per_model = None;
        let stats = Stats::new(Instant::now(), SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default());
        let (public_stats, _) = watch::channel(stats.snapshot(0, 0, 0));
        Scheduler {
            stats,
            public_stats,
            config,
            catalog: Arc::new(Catalog::parse(include_str!("../../../../tests/fixtures/all-models.json")).unwrap()),
            miners: HashMap::new(),
            jobs: HashMap::new(),
            queue: VecDeque::new(),
            tally: Tally::default(),
            counts: HashMap::new(),
            rotation: 0,
            admission: Admission::new(Some(12)),
        }
    }

    pub(crate) fn miner(scheduler: &mut Scheduler, session: &str, profile: &str) -> mpsc::Receiver<PoolMessage> {
        miner_model(scheduler, session, "clef", profile)
    }

    pub(crate) fn miner_model(
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
            runtime: None,
        };
        scheduler.on_miner_message(
            session,
            MinerMessage::Capacity { devices: vec![DeviceSlots { id: "gpu0".into(), slots: 1 }], models: vec![loaded] },
        );
        receiver
    }

    pub(crate) fn job(scheduler: &Scheduler, id: &str, counted: u32, minimum: Option<u32>) -> NewJob {
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

    pub(crate) fn speed(scheduler: &mut Scheduler, session: &str, rate: f64, slots: u32) {
        let miner = scheduler.miners.get_mut(session).unwrap();
        miner.devices[0].slots = slots;
        let performance = miner.performance.entry("gpu0".into()).or_default();
        performance.tokens_per_second = Some(rate);
        performance.last_sample = Some(Instant::now());
        performance.idle_since = Some(Instant::now());
    }

    pub(crate) fn enqueue(scheduler: &mut Scheduler, id: &str, tokens: u32) -> mpsc::Receiver<JobEvent> {
        let (events, receiver) = mpsc::channel(8);
        scheduler.admit(job(scheduler, id, tokens, None), events).unwrap();
        receiver
    }

    pub(crate) fn selected(scheduler: &Scheduler, id: &str, rotation: usize) -> String {
        scheduler.find_slot(&scheduler.jobs[id], rotation).unwrap().0
    }
}
