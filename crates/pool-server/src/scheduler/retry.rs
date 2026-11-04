//! Bound infrastructure retries while honoring caller-owned completion budgets.

use super::*;

impl Scheduler {
    pub(super) fn can_retry(&self, job_id: &str, failed_session: &str) -> bool {
        let job = &self.jobs[job_id];
        !job.committed
            && job.attempts < job.new.request.routing.max_attempts.unwrap_or(MAX_RETRIES + 1)
            && Instant::now() < job.deadline
            && self.miners.iter().any(|(session, miner)| {
                session != failed_session
                    && !job.excluded.contains(session)
                    && !miner.draining
                    && miner.serves(&job.new.model, &job.new.model_revision).any(|loaded| {
                        loaded.context_tokens >= job.new.routing_context()
                            && runtime_matches(loaded, job.new.request.routing.runtime_fingerprint.as_deref())
                    })
            })
    }

    pub(super) fn on_job_error(&mut self, job_id: &str, session: &str, code: JobErrorCode, message: String) {
        let retry = code.retryable() && self.can_retry(job_id, session);
        let job = self.jobs.get_mut(job_id).expect("released job exists");
        if retry {
            job.excluded.insert(session.into());
            job.state = JobState::Queued;
            self.tally.deactivate(&job.new.client, &job.new.model, job.new.request.question_count);
            self.queue.push_front(job_id.into());
            return;
        }
        let error = match code {
            JobErrorCode::RevisionMismatch => revision_mismatch(job.new.request.routing.runtime_fingerprint.as_deref()),
            JobErrorCode::OutOfMemory => ApiError::bad_request("runtime_out_of_memory", message),
            JobErrorCode::InvalidRequest => ApiError::bad_request("invalid_request", message),
            JobErrorCode::ContextLengthExceeded => ApiError::bad_request("context_length_exceeded", message),
            JobErrorCode::Timeout => ApiError::new(StatusCode::GATEWAY_TIMEOUT, "inference_timeout", message),
            JobErrorCode::RuntimeFailed | JobErrorCode::Internal => ApiError::miner_failed(message),
        };
        self.fail(job_id, error);
    }

    pub(super) fn requeue_rejected(&mut self, job_id: &str, session: &str, reason: RejectReason) {
        let job = self.jobs.get_mut(job_id).expect("released job exists");
        // The dispatch never ran, regardless of why the miner rejected it.
        job.attempts = job.attempts.saturating_sub(1);
        if reason == RejectReason::RevisionMismatch {
            let error = revision_mismatch(self.jobs[job_id].new.request.routing.runtime_fingerprint.as_deref());
            self.fail(job_id, error);
            return;
        }
        let job = self.jobs.get_mut(job_id).expect("released job exists");
        job.rejections += 1;
        let rejections = job.rejections;
        if rejections > MAX_REJECTIONS || !self.can_retry(job_id, session) {
            self.fail(
                job_id,
                ApiError::miner_failed(format!("no miner could take the job (last rejection: {reason:?})")),
            );
            return;
        }
        let job = self.jobs.get_mut(job_id).expect("released job exists");
        job.excluded.insert(session.into());
        job.state = JobState::Queued;
        self.tally.deactivate(&job.new.client, &job.new.model, job.new.request.question_count);
        self.queue.push_front(job_id.into());
    }
}

#[cfg(test)]
mod tests {
    use super::super::snapshot::test_support::*;
    use super::super::*;
    use pool_protocol::requests;
    #[tokio::test(start_paused = true)]
    async fn mixed_h2o_question_counts_use_total_work_but_keep_prompt_context_and_stats() {
        let mut scheduler = scheduler();
        let mut slow = miner_model(&mut scheduler, "slow", "h2o-q8", "cuda-8192");
        let mut fast = miner_model(&mut scheduler, "fast", "h2o-q8", "cuda-40960-x6");
        speed(&mut scheduler, "slow", 1000.0, 2);
        speed(&mut scheduler, "fast", 4000.0, 12);
        let mut receivers = Vec::new();
        for (id, count, expected) in [("multi", 4, "fast"), ("single", 1, "slow")] {
            let model = scheduler.catalog.model("h2o-q8").unwrap();
            let questions: serde_json::Map<String, Value> =
                (0..count).map(|i| (i.to_string(), json!({"type": "noul", "instructions": "test"}))).collect();
            let payload = json!({"state": "test", "questions": questions});
            let new = NewJob {
                id: id.into(),
                client: "client".into(),
                model: model.id.clone(),
                model_revision: scheduler.catalog.model_revision(model),
                input_tokens: 1000,
                required_context: 1000,
                request: requests::validate(Operation::Systemone, model, payload.as_object().unwrap().clone()).unwrap(),
            };
            assert_eq!(new.work_tokens(), 1000 * count);
            assert_eq!(new.routing_context(), 1000);
            let (events, receiver) = mpsc::channel(8);
            receivers.push(receiver);
            scheduler.admit(new, events).unwrap();
            assert_eq!(selected(&scheduler, id, 0), expected);
            scheduler.pump();
        }
        assert_eq!(scheduler.miners["fast"].performance["gpu0"].outstanding_tokens, 4000);
        assert_eq!(scheduler.miners["slow"].performance["gpu0"].outstanding_tokens, 1000);
        tokio::time::advance(Duration::from_secs(1)).await;
        for (session, outbound, expected_rate) in [("fast", &mut fast, 4000.0), ("slow", &mut slow, 1000.0)] {
            let PoolMessage::Job { attempt_id, input_tokens, required_context_tokens, .. } =
                outbound.try_recv().unwrap()
            else {
                panic!("expected job")
            };
            assert_eq!((input_tokens, required_context_tokens), (1000, 1000));
            scheduler.on_miner_message(session, MinerMessage::Result { attempt_id, body: json!({}), runtime: None });
            assert_eq!(scheduler.miners[session].performance["gpu0"].tokens_per_second, Some(expected_rate));
            assert_eq!(scheduler.miners[session].performance["gpu0"].outstanding_tokens, 0);
        }
        let stats = scheduler.stats.snapshot(0, 0, 0);
        assert_eq!(stats.average_prompt_tokens, Some(1000.0));
        assert_eq!(stats.answered_per_second, 5.0 / 60.0);
    }

    #[test]
    fn immediately_dispatched_questions_do_not_raise_waiting_peaks() {
        let mut scheduler = scheduler();
        let now = Instant::now();
        scheduler.stats = Stats::new(now, Duration::from_secs(120));
        let _miner = miner(&mut scheduler, "miner", "cuda-4096");
        scheduler.advance_stats(now + Duration::from_secs(60));
        scheduler.publish();
        let (events, _receiver) = mpsc::channel(1);
        let mut new = job(&scheduler, "active", 1234, None);
        new.request.question_count = 4;
        scheduler.admit(new, events).unwrap();
        scheduler.pump();
        scheduler.debug_check();
        scheduler.publish();
        assert_eq!(scheduler.public_stats.borrow().queued_questions, 0);
        assert_eq!(scheduler.public_stats.borrow().answering_questions, 4);
        scheduler.advance_stats(now + Duration::from_secs(61));
        scheduler.publish();
        assert_eq!(scheduler.public_stats.borrow().live.last().unwrap().queued, 0);
        assert_eq!(scheduler.public_stats.borrow().live.last().unwrap().answering, 4);
        scheduler.drop_job("active", None);
        scheduler.debug_check();
        scheduler.publish();
        assert_eq!(scheduler.public_stats.borrow().answering_questions, 0);
        assert_eq!(scheduler.public_stats.borrow().average_prompt_tokens, None);
        scheduler.advance_stats(now + Duration::from_secs(120));
        scheduler.publish();
        assert_eq!(scheduler.public_stats.borrow().history.queued, [0]);
    }

    #[test]
    fn waiting_minimum_jobs_use_normal_queue_capacity_and_admission() {
        let mut scheduler = scheduler();
        let _small = miner(&mut scheduler, "small", "cuda-4096");
        assert_eq!(scheduler.queue_capacities("clef"), (16, 16));
        scheduler.config.max_active_per_client = None;
        scheduler.config.max_queued_per_client = None;
        for index in 0..16 {
            let (events, _receiver) = mpsc::channel(1);
            scheduler.admit(job(&scheduler, &format!("wait{index}"), 100, Some(8192)), events).unwrap();
        }
        let (events, _receiver) = mpsc::channel(1);
        assert_eq!(
            scheduler.admit(job(&scheduler, "full", 100, Some(8192)), events).unwrap_err().code,
            "pool_overloaded"
        );
        scheduler.publish();
        assert_eq!(scheduler.admission.reserve("other").err().unwrap().code, "pool_overloaded");
    }
}
