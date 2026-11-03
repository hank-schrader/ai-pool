//! Keep public model/request identities while preserving assistant content, finish reasons and usage.

use super::*;

/// Client-facing names: the API model id and a stable completion id, never the
/// runtime's file name or internal ids.
pub(super) fn shape_response(job: &Job, mut body: Value) -> Value {
    if let Some(object) = body.as_object_mut() {
        object.insert("model".into(), job.new.model.clone().into());
        if job.new.request.operation == Operation::ChatCompletions {
            object.insert("id".into(), completion_id(job).into());
            object.insert("created".into(), job.created_unix.into());
            object.remove("timings");
        }
    }
    body
}

pub(super) fn shape_chunk(job: &Job, mut chunk: Value) -> Value {
    if let Some(object) = chunk.as_object_mut() {
        object.insert("id".into(), completion_id(job).into());
        object.insert("model".into(), job.new.model.clone().into());
        object.insert("created".into(), job.created_unix.into());
        object.remove("timings");
    }
    chunk
}

fn completion_id(job: &Job) -> String {
    format!("chatcmpl-{}", job.new.id.trim_start_matches("req_"))
}

#[cfg(test)]
mod tests {
    use super::super::snapshot::test_support::*;
    use super::super::*;
    use pool_protocol::requests;
    #[test]
    fn terminal_success_counts_only_when_enqueued_in_the_client_channel() {
        for (stream, include_usage, full, closed) in [
            (false, false, false, false),
            (false, false, false, true),
            (true, false, false, false),
            (true, false, false, true),
            (true, false, true, false),
            (true, true, false, false),
            (true, true, true, false),
        ] {
            let mut scheduler = scheduler();
            let now = Instant::now();
            scheduler.stats = Stats::new(now, Duration::from_secs(120));
            scheduler.advance_stats(now + Duration::from_secs(60));
            let mut outbound = miner_model(&mut scheduler, "miner", "qwen2.5-1.5b-instruct", "cuda-4096");
            let model = scheduler.catalog.model("qwen2.5-1.5b-instruct").unwrap();
            let body = json!({"messages": [{"role": "user", "content": "hi"}], "stream": stream,
                "stream_options": {"include_usage": include_usage}});
            let mut fields = body.as_object().unwrap().clone();
            if !stream {
                fields.remove("stream_options");
            }
            let request = requests::validate(Operation::ChatCompletions, model, fields).unwrap();
            let job = NewJob {
                id: "terminal".into(),
                client: "client".into(),
                model: model.id.clone(),
                model_revision: scheduler.catalog.model_revision(model),
                required_context: request.required_context(100),
                input_tokens: 100,
                request,
            };
            let (events, receiver) = mpsc::channel(1);
            let mut receiver = Some(receiver);
            scheduler.admit(job, events).unwrap();
            scheduler.pump();
            let PoolMessage::Job { attempt_id, .. } = outbound.try_recv().unwrap() else { panic!("expected job") };
            if closed {
                receiver.take();
            }
            if full {
                scheduler.on_miner_message(
                    "miner",
                    MinerMessage::StreamChunk { attempt_id: attempt_id.clone(), seq: 0, chunk: json!({"choices": []}) },
                );
            }
            let terminal = if stream {
                MinerMessage::StreamEnd { attempt_id, usage: Some(json!({"completion_tokens": 1})) }
            } else {
                MinerMessage::Result { attempt_id, body: json!({"choices": []}), runtime: None }
            };
            scheduler.on_miner_message("miner", terminal);
            let delivered = !closed && !full && !include_usage;
            assert!(scheduler.jobs.is_empty());
            scheduler.debug_check();
            assert_eq!(scheduler.tally.answering_questions, 0);
            assert_eq!(scheduler.stats.snapshot(0, 0, 0).average_prompt_tokens, delivered.then_some(100.0));
            assert_eq!(scheduler.stats.snapshot(0, 0, 0).answered_per_second, u64::from(delivered) as f64 / 60.0);
            assert_eq!(scheduler.stats.snapshot(0, 0, 0).answered_last_hour, u64::from(delivered));
            if let Some(mut receiver) = receiver {
                let event = receiver.try_recv().unwrap();
                if delivered {
                    assert!(matches!(event, JobEvent::End | JobEvent::Result { .. }));
                } else {
                    assert!(matches!(event, JobEvent::Chunk(_)));
                }
                assert!(matches!(receiver.try_recv(), Err(mpsc::error::TryRecvError::Disconnected)));
            }
            scheduler.advance_stats(now + Duration::from_secs(120));
            let snapshot = scheduler.stats.snapshot(0, 0, 0);
            assert_eq!(snapshot.answered_last_24h, u64::from(delivered));
            assert_eq!(snapshot.history.requests, [u64::from(delivered)]);
            assert_eq!(snapshot.history.prompt_tokens, [u64::from(delivered) * 100]);
        }
    }
    #[test]
    fn observation_window_rolls_over_after_sixty_busy_seconds_excluding_idle() {
        let mut performance = DevicePerformance::default();
        let mut now = Instant::now();
        let mut attempt = Attempt { job: Some("old".into()), device: "gpu0".into(), work_tokens: 6000 };
        performance.start(attempt.work_tokens, now);
        now += SPEED_WINDOW;
        performance.finish(&attempt, true, now);
        assert_eq!(performance.tokens_per_second, Some(100.0));
        // Idle beyond the observation window cannot evict busy history (keep
        // below the separate 300-second recovery-probe reset threshold).
        now += Duration::from_secs(120);
        attempt.work_tokens = 1000;
        for second in 1..=60 {
            performance.start(attempt.work_tokens, now);
            now += Duration::from_secs(1);
            performance.finish(&attempt, true, now);
            if second == 59 {
                assert!((performance.tokens_per_second.unwrap() - 65000.0 / 119.0).abs() < 0.001);
            }
        }
        assert_eq!(performance.tokens_per_second, Some(1000.0));
        assert_eq!(performance.samples.iter().map(|s| s.1).sum::<f64>(), 60.0);
        assert_eq!(performance.samples.iter().map(|s| s.0).sum::<u64>(), 60000);
        // Rollover continues with a bounded number of time buckets.
        for _ in 0..100 {
            performance.start(attempt.work_tokens, now);
            now += Duration::from_secs(1);
            performance.finish(&attempt, true, now);
        }
        assert_eq!(performance.tokens_per_second, Some(1000.0));
        assert_eq!(performance.samples.len(), 60);
    }

    #[test]
    fn minimum_context_dispatches_when_matching_capacity_frees() {
        let mut scheduler = scheduler();
        let mut large = miner(&mut scheduler, "large", "cuda-8192");
        let (events, _receiver) = mpsc::channel(2);
        scheduler.admit(job(&scheduler, "first", 100, Some(8192)), events.clone()).unwrap();
        scheduler.pump();
        let PoolMessage::Job { attempt_id, .. } = large.try_recv().unwrap() else { panic!("expected first job") };
        scheduler.admit(job(&scheduler, "second", 100, Some(8192)), events).unwrap();
        scheduler.pump();
        assert!(large.try_recv().is_err());
        assert_eq!(scheduler.queue.len(), 1);
        scheduler.on_miner_message(
            "large",
            MinerMessage::Result {
                attempt_id,
                body: json!({"answers": {"q": {"noul": 0.9}}, "usage": {"input_tokens": 100}}),
                runtime: None,
            },
        );
        assert!(matches!(large.try_recv().unwrap(), PoolMessage::Job { job_id, .. } if job_id == "second"));
    }
}
