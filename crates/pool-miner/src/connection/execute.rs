use super::*;

pub(super) async fn execute(
    ready: &Ready,
    operation: Operation,
    stream: bool,
    payload: &Value,
    attempt_id: &str,
    out: &mpsc::Sender<MinerMessage>,
) -> Result<(), RuntimeError> {
    let result =
        |body| MinerMessage::Result { attempt_id: attempt_id.to_string(), body, runtime: ready.runtime.clone() };
    match (operation, stream) {
        (Operation::Systemone, _) => {
            let body = ready.systemone(payload).await?;
            let _ = out.send(result(body)).await;
        }
        (Operation::ChatCompletions, false) => {
            let body = ready.client.chat(payload).await?;
            let _ = out.send(result(body)).await;
        }
        (Operation::ChatCompletions, true) => {
            let mut events = ready.client.chat_stream(payload).await?;
            let _ = out.send(MinerMessage::StreamStart { attempt_id: attempt_id.into() }).await;
            let mut seq = 0;
            let mut usage = None;
            loop {
                match events.next().await {
                    None => return Err(RuntimeError::new(JobErrorCode::RuntimeFailed, "stream ended without [DONE]")),
                    Some(Err(error)) => return Err(error),
                    Some(Ok(StreamItem::Done)) => break,
                    Some(Ok(StreamItem::Chunk(chunk))) => {
                        let usage_only = chunk.get("choices").and_then(Value::as_array).is_some_and(Vec::is_empty)
                            && chunk.get("usage").is_some();
                        if usage_only {
                            usage = chunk.get("usage").cloned();
                            continue;
                        }
                        // waiting here applies the pool's backpressure to the runtime
                        if out
                            .send(MinerMessage::StreamChunk { attempt_id: attempt_id.into(), seq, chunk })
                            .await
                            .is_err()
                        {
                            return Ok(());
                        }
                        seq += 1;
                    }
                }
            }
            let _ = out.send(MinerMessage::StreamEnd { attempt_id: attempt_id.into(), usage }).await;
        }
    }
    Ok(())
}

impl Miner {
    /// Whether this miner can run the job now; takes a device slot if so.
    pub(super) fn check_job(
        &self,
        model: &str,
        revision: &str,
        profile: &str,
        required_context: u32,
        fingerprint: Option<&str>,
    ) -> Result<(Ready, SlotGuard), (RejectReason, String)> {
        if self.draining.load(Ordering::SeqCst) {
            return Err((RejectReason::Draining, "miner is shutting down".into()));
        }
        let Some(hosted) = self.hosted(model) else {
            return Err((RejectReason::ModelUnavailable, format!("{model} is not served here")));
        };
        if hosted.revision != revision {
            return Err((RejectReason::RevisionMismatch, format!("serving {}", hosted.revision)));
        }
        let Some(ready) = hosted.ready() else {
            return Err((RejectReason::ModelUnavailable, "runtime is restarting".into()));
        };
        if fingerprint.is_some_and(|pin| ready.runtime.as_ref().is_none_or(|r| r.fingerprint != pin)) {
            return Err((RejectReason::RevisionMismatch, "runtime fingerprint mismatch".into()));
        }
        if ready.profile.id != profile {
            return Err((RejectReason::ModelUnavailable, format!("serving profile {}", ready.profile.id)));
        }
        if ready.profile.context_tokens < required_context {
            return Err((
                RejectReason::ContextTooSmall,
                format!("context {} < required {required_context}", ready.profile.context_tokens),
            ));
        }
        let slots = self.device_slots() as usize;
        // Rust 1.99 deprecates fetch_update for try_update, which older toolchains lack.
        #[allow(deprecated)]
        let taken = self.running.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| (n < slots).then_some(n + 1));
        if taken.is_err() {
            return Err((RejectReason::Busy, "all device slots are busy".into()));
        }
        let permit = SlotGuard(self.running.clone());
        Ok((ready, permit))
    }
}
