use super::*;

pub(super) struct CountGuard {
    pub(super) handle: SchedulerHandle,
    pub(super) count_id: String,
}
impl Drop for CountGuard {
    fn drop(&mut self) {
        self.handle.send(Command::CancelCount { count_id: self.count_id.clone() });
    }
}

impl Scheduler {
    pub(super) fn start_count(
        &mut self,
        id: String,
        request: CountRequest,
        reply: oneshot::Sender<Result<CountReply, ApiError>>,
    ) {
        if self.counts.len() >= MAX_PENDING_COUNTS {
            let _ = reply.send(Err(ApiError::overloaded("too many requests are being counted")));
            return;
        }
        self.send_count(id, request, reply, Instant::now() + self.config.count_timeout, &HashSet::new());
    }

    pub(super) fn send_count(
        &mut self,
        count_id: String,
        request: CountRequest,
        reply: oneshot::Sender<Result<CountReply, ApiError>>,
        deadline: Instant,
        excluded: &HashSet<String>,
    ) {
        if reply.is_closed() {
            return;
        }
        let selected = self
            .miners
            .iter()
            .filter(|(session, miner)| !miner.draining && !excluded.contains(*session))
            .flat_map(|(session, miner)| {
                miner
                    .serves(&request.model, &request.model_revision)
                    // Counting needs the tokenizer/template, not an execution slot
                    // large enough for min_context_tokens. Admission queues that wait.
                    .filter(|loaded| runtime_matches(loaded, request.runtime_fingerprint.as_deref()))
                    .map(move |loaded| (miner.pending_counts, session.clone(), loaded.clone()))
            })
            .min_by(|a, b| (&a.0, &a.1, &a.2.profile).cmp(&(&b.0, &b.1, &b.2.profile)));
        let Some((_, session, loaded)) = selected else {
            let _ = reply.send(Err(unavailable(&request.model, request.runtime_fingerprint.as_deref())));
            return;
        };
        let message = PoolMessage::CountTokens {
            count_id: count_id.clone(),
            model: request.model.clone(),
            model_revision: request.model_revision.clone(),
            runtime_fingerprint: loaded.runtime.as_ref().map(|r| r.fingerprint.clone()),
            timeout_ms: deadline.saturating_duration_since(Instant::now()).as_millis() as u64,
            operation: request.operation,
            payload: request.payload.clone(),
        };
        let miner = self.miners.get_mut(&session).expect("selected miner exists");
        if miner.outbound.try_send(message).is_err() {
            let _ = reply.send(Err(ApiError::overloaded("miner connection is congested")));
            return;
        }
        miner.pending_counts += 1;
        self.counts.insert(count_id, PendingCount { request, loaded, reply, session, deadline });
    }

    pub(super) fn count_result(&mut self, session: &str, id: &str, tokens: u32, reported: Option<LoadedModel>) {
        let Some(pending) = self.counts.get(id).filter(|p| p.session == session) else { return };
        if pending.loaded.runtime.is_some() && reported.is_none() {
            let error = revision_mismatch(pending.request.runtime_fingerprint.as_deref());
            self.finish_count(id, Err(error));
            return;
        }
        let loaded = reported.unwrap_or_else(|| pending.loaded.clone());
        let expected = &pending.loaded;
        let valid = loaded.model == expected.model
            && loaded.model_revision == expected.model_revision
            && loaded.profile == expected.profile
            && loaded.context_tokens == expected.context_tokens
            && loaded.runtime.as_ref().map(|r| &r.fingerprint) == expected.runtime.as_ref().map(|r| &r.fingerprint)
            && self.catalog.model(&loaded.model).is_some_and(|m| valid_runtime(&self.catalog, m, &loaded));
        if !valid {
            let error = revision_mismatch(pending.request.runtime_fingerprint.as_deref());
            self.finish_count(id, Err(error));
            return;
        }
        self.finish_count(id, Ok(CountReply { input_tokens: tokens, loaded }));
    }

    pub(super) fn finish_count(&mut self, id: &str, result: Result<CountReply, ApiError>) {
        if let Some(pending) = self.take_count(id) {
            if result.is_err()
                && let Some(miner) = self.miners.get(&pending.session)
            {
                let _ = miner.outbound.try_send(PoolMessage::CancelCount { count_id: id.into() });
            }
            let _ = pending.reply.send(result);
        }
    }

    fn take_count(&mut self, id: &str) -> Option<PendingCount> {
        let pending = self.counts.remove(id)?;
        if let Some(miner) = self.miners.get_mut(&pending.session) {
            miner.pending_counts = miner.pending_counts.saturating_sub(1);
        }
        Some(pending)
    }

    pub(super) fn cancel_count(&mut self, id: &str) {
        if let Some(pending) = self.take_count(id)
            && let Some(miner) = self.miners.get(&pending.session)
        {
            let _ = miner.outbound.try_send(PoolMessage::CancelCount { count_id: id.into() });
        }
    }
}
