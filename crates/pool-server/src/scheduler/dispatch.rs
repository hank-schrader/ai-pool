//! FIFO admission and dispatch with context, fingerprint and attempt limits.

use super::*;

impl Scheduler {
    pub(super) fn admit(&mut self, new: NewJob, events: mpsc::Sender<JobEvent>) -> Result<(), ApiError> {
        if !self.miners.values().any(|miner| {
            !miner.draining
                && miner
                    .serves(&new.model, &new.model_revision)
                    .any(|loaded| runtime_matches(loaded, new.request.routing.runtime_fingerprint.as_deref()))
        }) {
            return Err(unavailable(&new.model, new.request.routing.runtime_fingerprint.as_deref()));
        }
        let (client_active, client_queued) = self.tally.client(&new.client);
        let client_jobs = client_active + client_queued;
        if let (Some(active), Some(queued)) = (self.config.max_active_per_client, self.config.max_queued_per_client)
            && client_jobs >= active + queued
        {
            return Err(self.admission.rate_limited(&new.client, active + queued, 1));
        }
        let model_queued = self.tally.model_queued(&new.model);
        let (global_capacity, model_capacity) = self.queue_capacities(&new.model);
        if self.queue.len() >= global_capacity || model_queued >= model_capacity {
            return Err(self.admission.overloaded(self.queue.len().saturating_sub(global_capacity) + 1));
        }

        let now = Instant::now();
        let id = new.id.clone();
        debug!(job = %id, required_context = new.required_context, routing_context = new.routing_context(), "queued");
        self.tally.enqueue(&new.client, &new.model, new.request.question_count);
        self.jobs.insert(
            id.clone(),
            Job {
                created_unix: SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()),
                deadline: now + self.config.request_timeout,
                queue_deadline: now + self.config.queue_timeout,
                first_chunk_deadline: now + self.config.first_chunk_timeout,
                new,
                events,
                state: JobState::Queued,
                attempts: 0,
                rejections: 0,
                excluded: HashSet::new(),
                committed: false,
                next_seq: 0,
            },
        );
        self.queue.push_back(id);
        Ok(())
    }

    pub(super) fn queue_capacities(&self, model: &str) -> (usize, usize) {
        let mut all = 0usize;
        let mut serving = 0usize;
        for miner in self.miners.values().filter(|miner| !miner.draining) {
            all += miner.devices.iter().map(|d| d.slots as usize).sum::<usize>();
            let mut devices: Vec<&str> =
                miner.models.iter().filter(|m| m.model == model).map(|m| m.device.as_str()).collect();
            devices.sort_unstable();
            devices.dedup();
            serving += devices.into_iter().map(|device| miner.slots(device) as usize).sum::<usize>();
        }
        (
            self.config.queue_capacity_global.unwrap_or_else(|| (4 * all).max(16)),
            self.config.queue_capacity_per_model.unwrap_or_else(|| (4 * serving).max(16)),
        )
    }

    pub(super) fn pump(&mut self) {
        let mut index = 0;
        while index < self.queue.len() {
            let job_id = self.queue[index].clone();
            let job = &self.jobs[&job_id];
            if let Some(pin) = job.new.request.routing.runtime_fingerprint.as_deref()
                && !self.miners.values().any(|miner| {
                    !miner.draining
                        && miner
                            .serves(&job.new.model, &job.new.model_revision)
                            .any(|loaded| runtime_matches(loaded, Some(pin)))
                })
            {
                let error = revision_mismatch(Some(pin));
                self.fail(&job_id, error);
                continue;
            }
            let client_active = self.tally.client(&job.new.client).0;
            if self.config.max_active_per_client.is_some_and(|max| client_active >= max) {
                index += 1;
                continue;
            }
            let slot = self.find_slot(job, self.rotation);
            self.rotation = self.rotation.wrapping_add(1);
            match slot {
                Some((session, device, profile)) => {
                    self.queue.remove(index);
                    self.dispatch(&job_id, &session, &device, &profile);
                }
                None => index += 1,
            }
        }
        self.debug_check();
    }

    pub(super) fn dispatch(&mut self, job_id: &str, session: &str, device: &str, profile: &str) {
        let attempt = format!("att_{}", uuid::Uuid::new_v4().simple());
        let job = self.jobs.get_mut(job_id).expect("queued job exists");
        let now = Instant::now();
        let message = PoolMessage::Job {
            job_id: job_id.into(),
            attempt_id: attempt.clone(),
            model: job.new.model.clone(),
            model_revision: job.new.model_revision.clone(),
            profile: profile.into(),
            runtime_fingerprint: job.new.request.routing.runtime_fingerprint.clone(),
            operation: job.new.request.operation,
            stream: job.new.request.stream,
            input_tokens: job.new.input_tokens,
            required_context_tokens: job.new.required_context,
            timeout_ms: job.deadline.saturating_duration_since(now).as_millis() as u64,
            payload: job.new.request.payload.clone(),
        };
        let miner = self.miners.get_mut(session).expect("candidate miner exists");
        if miner.outbound.try_send(message).is_err() {
            warn!(%session, "miner outbound queue is full or closed; disconnecting it");
            self.queue.push_front(job_id.into());
            self.remove_miner(session, "miner stopped reading");
            return;
        }
        let runtime = miner
            .models
            .iter()
            .find(|loaded| loaded.model == job.new.model && loaded.profile == profile && loaded.device == device)
            .and_then(|loaded| loaded.runtime.clone());
        let work_tokens = job.new.work_tokens();
        miner.performance.entry(device.into()).or_default().start(work_tokens, now);
        miner
            .attempts
            .insert(attempt.clone(), Attempt { job: Some(job_id.into()), device: device.into(), work_tokens });
        debug!(job = %job_id, %attempt, %session, %profile, routing_context = job.new.routing_context(), "dispatched");
        job.attempts += 1;
        self.tally.activate(&job.new.client, &job.new.model, job.new.request.question_count);
        job.state = JobState::Dispatched {
            session: session.into(),
            attempt,
            profile: profile.into(),
            runtime,
            sent_at: now,
            accepted: false,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::super::snapshot::test_support::*;
    use super::super::*;
    #[tokio::test(start_paused = true)]
    async fn completions_update_speed_excluding_idle_time_and_duplicate_results() {
        let mut scheduler = scheduler();
        let mut outbound = miner(&mut scheduler, "miner", "cuda-4096");
        let _first = enqueue(&mut scheduler, "first", 1000);
        scheduler.pump();
        let PoolMessage::Job { attempt_id, .. } = outbound.try_recv().unwrap() else { panic!("expected job") };
        tokio::time::advance(Duration::from_secs(2)).await;
        let result = MinerMessage::Result { attempt_id, body: json!({}), runtime: None };
        scheduler.on_miner_message("miner", result.clone());
        scheduler.on_miner_message("miner", result);
        assert_eq!(scheduler.miners["miner"].performance["gpu0"].tokens_per_second, Some(500.0));
        tokio::time::advance(Duration::from_secs(30)).await;
        let _second = enqueue(&mut scheduler, "second", 1000);
        scheduler.pump();
        let PoolMessage::Job { attempt_id, .. } = outbound.try_recv().unwrap() else { panic!("expected job") };
        tokio::time::advance(Duration::from_secs(1)).await;
        scheduler.on_miner_message("miner", MinerMessage::StreamEnd { attempt_id, usage: None });
        assert_eq!(scheduler.miners["miner"].performance["gpu0"].tokens_per_second, Some(2000.0 / 3.0));
        assert_eq!(scheduler.miners["miner"].performance["gpu0"].outstanding_tokens, 0);
    }

    #[test]
    #[should_panic(expected = "answering question counter drifted")]
    fn debug_check_detects_answering_question_drift() {
        let mut scheduler = scheduler();
        scheduler.tally.answering_questions = 1;
        scheduler.debug_check();
    }
}
