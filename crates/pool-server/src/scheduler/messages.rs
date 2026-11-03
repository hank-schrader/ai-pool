//! Validate miner counts, capacity and results before updating client-visible state.

use super::*;

impl Scheduler {
    pub(super) fn on_miner_message(&mut self, session: &str, message: MinerMessage) {
        let Some(miner) = self.miners.get_mut(session) else { return };
        miner.last_seen = Instant::now();
        match message {
            MinerMessage::Capacity { devices, models } => self.on_capacity(session, devices, models),
            MinerMessage::Heartbeat { .. } | MinerMessage::Hello { .. } => {}
            MinerMessage::Draining => {
                info!(%session, "miner is draining");
                if let Some(miner) = self.miners.get_mut(session) {
                    miner.draining = true;
                }
            }
            MinerMessage::CountResult { count_id, input_tokens, loaded } => {
                self.count_result(session, &count_id, input_tokens, loaded)
            }
            MinerMessage::CountError { count_id, code, message } => {
                if self.counts.get(&count_id).is_none_or(|p| p.session != session) {
                    return;
                }
                let error = match code {
                    JobErrorCode::RevisionMismatch => {
                        revision_mismatch(self.counts[&count_id].request.runtime_fingerprint.as_deref())
                    }
                    JobErrorCode::OutOfMemory => ApiError::bad_request("runtime_out_of_memory", message),
                    JobErrorCode::InvalidRequest => ApiError::bad_request("invalid_request", message),
                    JobErrorCode::ContextLengthExceeded => ApiError::bad_request("context_length_exceeded", message),
                    _ => ApiError::miner_failed(format!("token counting failed: {message}")),
                };
                self.finish_count(&count_id, Err(error));
            }
            MinerMessage::Accepted { attempt_id } => {
                if let Some(job) = self.job_for(session, &attempt_id)
                    && let JobState::Dispatched { accepted, .. } = &mut job.state
                {
                    *accepted = true;
                }
            }
            MinerMessage::Rejected { attempt_id, reason, message } => {
                debug!(%session, attempt = %attempt_id, ?reason, %message, "miner rejected job");
                if let Some(job_id) = self.release(session, &attempt_id, false) {
                    self.requeue_rejected(&job_id, session, reason);
                }
            }
            MinerMessage::Result { attempt_id, body, runtime } => {
                let valid = self.job_for(session, &attempt_id).is_some_and(|job| match &job.state {
                    JobState::Dispatched { runtime: expected, .. } => {
                        expected.as_ref().map(|r| &r.fingerprint) == runtime.as_ref().map(|r| &r.fingerprint)
                            && runtime.as_ref().is_none_or(|r| r.fingerprint == r.computed_fingerprint())
                    }
                    _ => false,
                });
                if !valid {
                    if let Some(job_id) = self.release(session, &attempt_id, false) {
                        let error =
                            revision_mismatch(self.jobs[&job_id].new.request.routing.runtime_fingerprint.as_deref());
                        self.fail(&job_id, error);
                    }
                    return;
                }
                if let Some(job_id) = self.release(session, &attempt_id, true) {
                    let job = self.remove_job(&job_id).expect("released job exists");
                    let profile = match &job.state {
                        JobState::Dispatched { profile, .. } => profile.clone(),
                        JobState::Queued => String::new(),
                    };
                    let body = shape_response(&job, body);
                    if job.events.try_send(JobEvent::Result { body, profile, runtime, attempts: job.attempts }).is_ok()
                    {
                        self.stats.answered(job.new.request.question_count, job.new.input_tokens);
                    }
                }
            }
            MinerMessage::StreamStart { .. } => {}
            MinerMessage::StreamChunk { attempt_id, seq, chunk } => self.on_chunk(session, &attempt_id, seq, chunk),
            MinerMessage::StreamEnd { attempt_id, usage } => {
                if let Some(job_id) = self.release(session, &attempt_id, true) {
                    let job = self.remove_job(&job_id).expect("released job exists");
                    if job.new.request.include_usage
                        && let Some(usage) = usage
                    {
                        let chunk = shape_chunk(
                            &job,
                            json!({"object": "chat.completion.chunk", "choices": [], "usage": usage}),
                        );
                        let _ = job.events.try_send(JobEvent::Chunk(chunk));
                    }
                    if job.events.try_send(JobEvent::End).is_ok() {
                        self.stats.answered(job.new.request.question_count, job.new.input_tokens);
                    }
                }
            }
            MinerMessage::JobError { attempt_id, code, message } => {
                warn!(%session, attempt = %attempt_id, ?code, %message, "job failed on miner");
                if let Some(job_id) = self.release(session, &attempt_id, false) {
                    self.on_job_error(&job_id, session, code, message);
                }
            }
            MinerMessage::Cancelled { attempt_id } => {
                self.release(session, &attempt_id, false);
            }
        }
        self.pump();
    }

    pub(super) fn on_capacity(&mut self, session: &str, devices: Vec<DeviceSlots>, models: Vec<LoadedModel>) {
        let mut accepted = Vec::new();
        for loaded in models {
            let valid = self.catalog.model(&loaded.model).is_some_and(|model| {
                self.catalog.model_revision(model) == loaded.model_revision
                    && model.profile(&loaded.profile).is_some_and(|p| p.context_tokens == loaded.context_tokens)
                    && valid_runtime(&self.catalog, model, &loaded)
            }) && devices.iter().any(|device| device.id == loaded.device);
            if valid {
                accepted.push(loaded);
            } else {
                warn!(%session, model = %loaded.model, profile = %loaded.profile, "ignoring a loaded model that does not match the catalog");
            }
        }
        let miner = self.miners.get_mut(session).expect("checked by caller");
        info!(
            %session,
            label = %miner.label,
            models = ?accepted.iter().map(|m| format!("{}@{}", m.model, m.profile)).collect::<Vec<_>>(),
            "miner capacity"
        );
        miner.devices = devices;
        miner.models = accepted;
    }

    pub(super) fn on_chunk(&mut self, session: &str, attempt_id: &str, seq: u64, chunk: Value) {
        let Some(job) = self.job_for(session, attempt_id) else { return };
        if seq != job.next_seq {
            let job_id = job.new.id.clone();
            self.drop_job(&job_id, Some(ApiError::miner_failed("miner sent stream chunks out of order")));
            return;
        }
        job.next_seq += 1;
        job.committed = true;
        let chunk = shape_chunk(job, chunk);
        match job.events.try_send(JobEvent::Chunk(chunk)) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                let job_id = job.new.id.clone();
                warn!(job = %job_id, "client is not reading the stream; cancelling");
                self.drop_job(&job_id, None);
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                let job_id = job.new.id.clone();
                self.drop_job(&job_id, None);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::snapshot::test_support::*;
    use super::super::*;
    #[test]
    fn context_waits_skips_small_jobs_and_dispatches_on_capacity() {
        for (counted, minimum) in [(100, Some(8192)), (5000, None)] {
            let mut scheduler = scheduler();
            let mut small = miner(&mut scheduler, "small", "cuda-4096");
            let (events, _receiver) = mpsc::channel(1);
            scheduler.admit(job(&scheduler, "waiting", counted, minimum), events.clone()).unwrap();
            scheduler.pump();
            assert!(small.try_recv().is_err());
            assert_eq!(scheduler.queue.len(), 1);
            scheduler.admit(job(&scheduler, "short", 100, None), events).unwrap();
            scheduler.pump();
            assert!(matches!(small.try_recv().unwrap(), PoolMessage::Job { job_id, .. } if job_id == "short"));
            assert_eq!(scheduler.queue.len(), 1);
            let mut large = miner(&mut scheduler, "large", "cuda-16384");
            assert!(
                matches!(large.try_recv().unwrap(), PoolMessage::Job { job_id, required_context_tokens, .. } if job_id == "waiting" && required_context_tokens == counted)
            );
            assert!(scheduler.queue.is_empty());
            assert_eq!(scheduler.jobs["waiting"].new.routing_context(), counted.max(minimum.unwrap_or(0)));
        }
    }

    #[test]
    fn unlimited_clients_are_never_refused_but_a_full_queue_is() {
        let admission = Admission::new(None);
        let held: Vec<_> = (0..100).map(|_| admission.reserve("a").unwrap()).collect();
        assert_eq!(held.len(), 100);
        admission.queue_capacity.store(4, std::sync::atomic::Ordering::Relaxed);
        admission.queue_len.store(4, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(admission.reserve("a").err().unwrap().code, "pool_overloaded");
    }
}
