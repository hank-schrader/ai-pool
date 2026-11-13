use super::*;

impl Miner {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn start_count(
        self: &Arc<Self>,
        count_id: String,
        model: String,
        revision: String,
        fingerprint: Option<String>,
        timeout_ms: u64,
        payload: Value,
        out: &mpsc::Sender<MinerMessage>,
    ) {
        let miner = self.clone();
        let out = out.clone();
        let id = count_id.clone();
        let mut handles = self.active_counts.lock().unwrap();
        // Bound queued work as well as the requests inside the runtime.
        if handles.len() >= 256 {
            let _ = out.try_send(MinerMessage::CountError {
                count_id,
                code: JobErrorCode::RuntimeFailed,
                message: "too many token counts".into(),
            });
            return;
        }
        let handle = tokio::spawn(async move {
            let work = async {
                let _permit = miner
                    .counts
                    .acquire()
                    .await
                    .map_err(|_| RuntimeError::new(JobErrorCode::Internal, "counter closed"))?;
                let hosted = miner
                    .hosted(&model)
                    .filter(|h| h.revision == revision)
                    .ok_or_else(|| RuntimeError::new(JobErrorCode::RevisionMismatch, "model revision mismatch"))?;
                let ready = hosted
                    .ready()
                    .ok_or_else(|| RuntimeError::new(JobErrorCode::RuntimeFailed, "runtime unavailable"))?;
                if fingerprint.as_ref().is_some_and(|pin| ready.runtime.as_ref().is_none_or(|r| &r.fingerprint != pin))
                {
                    return Err(RuntimeError::new(JobErrorCode::RevisionMismatch, "runtime fingerprint mismatch"));
                }
                let input_tokens =
                    if matches!(hosted.model.backend, Backend::LlamaServerSystemone | Backend::LlamaServerOpenjev) {
                        hosted.count(&payload).await?
                    } else {
                        hosted.count_ready(&payload, &ready).await?
                    };
                let loaded = pool_protocol::messages::LoadedModel {
                    model: model.clone(),
                    model_revision: revision,
                    profile: ready.profile.id,
                    device: hosted.device.clone(),
                    context_tokens: ready.profile.context_tokens,
                    runtime: ready.runtime,
                };
                Ok((input_tokens, loaded))
            };
            let result = tokio::time::timeout(Duration::from_millis(timeout_ms.min(300_000)), work).await;
            let result =
                result.unwrap_or_else(|_| Err(RuntimeError::new(JobErrorCode::Timeout, "token counting timed out")));
            let reply = match result {
                Ok((input_tokens, loaded)) => {
                    MinerMessage::CountResult { count_id: id.clone(), input_tokens, loaded: Some(loaded) }
                }
                Err(error) => {
                    MinerMessage::CountError { count_id: id.clone(), code: error.code, message: error.message }
                }
            };
            let _ = out.send(reply).await;
            miner.active_counts.lock().unwrap().remove(&id);
        });
        handles.insert(count_id, handle);
    }
}
