use serde_json::json;

use super::{
    engine, float,
    json::{self, Layout},
    prompt,
};

#[test]
fn floats_print_like_python_repr() {
    for (value, text) in [
        (0.1, "0.1"),
        (100.0, "100.0"),
        (-0.0, "-0.0"),
        (1e16, "1e+16"),
        (1.5e-5, "1.5e-05"),
        (0.0001, "0.0001"),
        (123456789012345680.0, "1.2345678901234568e+17"),
        (f64::INFINITY, "Infinity"),
    ] {
        assert_eq!(float::render(value), text);
    }
}

#[test]
fn json_layouts_match_python_dumps() {
    let value = json!({"a": [1, 2.5, "\u{e9}"], "b": null, "c": {}});
    assert_eq!(json::render(&value, Layout::Compact), r#"{"a":[1,2.5,"é"],"b":null,"c":{}}"#);
    assert_eq!(json::render(&value, Layout::Description), r#"{"a": [1, 2.5, "é"], "b": null, "c": {}}"#);
    assert_eq!(
        json::render(&json!({"a": [1], "b": "\u{e9}"}), Layout::Instructions),
        "{\n \"a\": [\n  1\n ],\n \"b\": \"\\u00e9\"\n}"
    );
}

#[test]
fn prompt_is_the_chatml_answer_slot_text() {
    let question = json!({
        "type": "choice",
        "instructions": "  Which team should handle this?",
        "criteria": {"billing": null, "shipping": "parcels, \"late\" ones"}
    });
    let text = prompt::render(&json!("Customer: charged twice."), &question).unwrap();
    assert_eq!(
        text,
        "<|im_start|>system\nYou are a decision engine. You read a record and answer one question about it by \
         choosing exactly one option. Reply with a single letter.<|im_end|>\n<|im_start|>user\nrecord: Customer: \
         charged twice.\nquestion: Which team should handle this?\noptions:\nA) billing: billing\nB) shipping: \
         parcels, \"late\" ones<|im_end|>\n<|im_start|>assistant\n<think>\n\n</think>\n\nAnswer:"
    );
}

#[test]
fn noul_and_score_options_follow_the_original() {
    let (names, descriptions) = prompt::options(&json!({"type": "noul", "instructions": "x"})).unwrap();
    assert_eq!(names, ["true", "false"]);
    assert_eq!(descriptions, ["the statement holds", "it does not"]);
    let (names, _) = prompt::options(&json!({"type": "score", "criteria": ["low", "high"]})).unwrap();
    assert_eq!(names, ["0", "1"]);
    let too_many = json!({"type": "score", "criteria": vec!["x"; 17]});
    assert!(prompt::options(&too_many).is_err());
    assert!(prompt::options(&json!({"type": "ranking"})).is_err());
}

#[test]
fn probabilities_and_noul_commit() {
    let p = engine::probabilities(&[0.9_f64.ln(), 0.1_f64.ln()], 0.8);
    assert!((p.iter().sum::<f64>() - 1.0).abs() < 1e-12);
    assert!(p[0] > 0.9);
    assert_eq!(engine::commit_noul(0.95), 0.95);
    assert_eq!(engine::commit_noul(0.65), 0.801);
    assert!((engine::commit_noul(0.4) - 0.199).abs() < 1e-12);
    assert_eq!(engine::commit_noul(0.5), 0.801);
}

#[test]
fn completion_body_biases_only_the_labels() {
    let body = String::from_utf8(engine::completion_body(&[5, 6], &[10, 11], 2)).unwrap();
    let value: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(value["prompt"], json!([5, 6]));
    assert_eq!(value["logit_bias"], json!([[10, 64.0], [11, 64.0]]));
    assert_eq!(value["n_probs"], 2);
    assert_eq!(value["temperature"], 1.0);
    assert!(body.contains("\"temperature\":1.0,"));
    assert_eq!(value["id_slot"], 2);
    assert_eq!(value["cache_prompt"], true);
    assert_eq!(value["n_predict"], 1);
    assert_eq!(value["samplers"], json!([]));
}

mod slots {
    use std::time::Duration;

    use serde_json::{Value, json};
    use tokio::{
        sync::{mpsc, oneshot, watch},
        task::JoinHandle,
    };

    use crate::{H2o, LocalClient, RuntimeError, client::FakeRequest};

    struct Completion {
        body: Value,
        reply: oneshot::Sender<Result<Value, RuntimeError>>,
    }

    impl Completion {
        fn slot(&self) -> u64 {
            self.body["id_slot"].as_u64().unwrap()
        }

        fn finish(self) {
            let labels = self.body["logit_bias"].as_array().unwrap();
            let top: Vec<_> = labels
                .iter()
                .enumerate()
                .map(|(index, label)| {
                    json!({
                        "id": label[0], "prob": if index == 0 { 0.9 } else { 0.1 / (labels.len() - 1) as f64 },
                    })
                })
                .collect();
            let _ = self.reply.send(Ok(json!({
                "tokens_evaluated": self.body["prompt"].as_array().unwrap().len(), "truncated": false,
                "completion_probabilities": [{"top_probs": top}],
            })));
        }
    }

    // In-memory server transport uses the actual serialized completion body,
    // with responses held by the test so concurrency checks need no sleeps or sockets.
    struct Server {
        completions: mpsc::UnboundedReceiver<Completion>,
        tokenized: watch::Receiver<u64>,
        task: JoinHandle<()>,
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    impl Server {
        async fn start(slots: u32) -> (Self, H2o) {
            let (sender, mut requests) = mpsc::unbounded_channel::<FakeRequest>();
            let (completion, completions) = mpsc::unbounded_channel();
            let (count, tokenized) = watch::channel(0u64);
            let task = tokio::spawn(async move {
                while let Some(request) = requests.recv().await {
                    let body: Value = serde_json::from_slice(&request.body).unwrap();
                    match request.path.as_str() {
                        "/tokenize" => {
                            let text = body["content"].as_str().unwrap();
                            let bytes = text.as_bytes();
                            let mut tokens: Vec<u32> = bytes.iter().map(|byte| u32::from(*byte)).collect();
                            if bytes.len() >= 2
                                && bytes[bytes.len() - 2] == b' '
                                && (b'A'..=b'P').contains(bytes.last().unwrap())
                            {
                                tokens.truncate(tokens.len() - 2);
                                tokens.push(1000 + u32::from(bytes[bytes.len() - 1] - b'A'));
                            }
                            let _ = request.reply.send(Ok(json!({"tokens": tokens})));
                            count.send_modify(|count| *count += 1);
                        }
                        "/completion" => {
                            let _ = completion.send(Completion { body, reply: request.reply });
                        }
                        _ => panic!("unexpected fake-server route"),
                    }
                }
            });
            let engine = H2o::start(LocalClient::fake(sender), 8192, slots).await.unwrap();
            (Self { completions, tokenized, task }, engine)
        }

        async fn next(&mut self) -> Completion {
            tokio::time::timeout(Duration::from_secs(2), self.completions.recv()).await.unwrap().unwrap()
        }

        async fn prepared(&mut self, count: u64) {
            tokio::time::timeout(Duration::from_secs(2), async {
                while *self.tokenized.borrow_and_update() < count {
                    self.tokenized.changed().await.unwrap();
                }
            })
            .await
            .unwrap();
        }
    }

    fn payload(state: &str, count: usize) -> Value {
        let questions: serde_json::Map<_, _> = (0..count)
            .map(|index| {
                (
                    format!("q{index}"),
                    if index % 2 == 0 {
                        json!({"type": "noul", "instructions": "Is this a record?"})
                    } else {
                        json!({"type": "choice", "instructions": "Which?", "criteria": {"first": null, "second": null}})
                    },
                )
            })
            .collect();
        json!({"state": state, "questions": questions})
    }

    fn answer(engine: &H2o, body: Value) -> JoinHandle<Result<Value, RuntimeError>> {
        let engine = engine.clone();
        tokio::spawn(async move { engine.answer(&body).await })
    }

    #[tokio::test]
    async fn questions_keep_one_slot_and_preserve_answers_and_usage() {
        let (mut server, engine) = Server::start(2).await;
        let body = payload("shared record", 3);
        let job = answer(&engine, body.clone());
        let mut slot = None;
        let mut input_tokens = 0;
        for (id, question) in body["questions"].as_object().unwrap() {
            let completion = server.next().await;
            let expected: Vec<_> =
                super::prompt::render(&body["state"], question).unwrap().bytes().map(u32::from).collect();
            assert_eq!(completion.body["prompt"], json!(expected));
            assert_eq!(completion.body["cache_prompt"], true);
            assert!(completion.slot() < 2);
            assert_eq!(*slot.get_or_insert(completion.slot()), completion.slot());
            input_tokens += expected.len();
            assert!(id.starts_with('q'));
            completion.finish();
        }
        let reply = job.await.unwrap().unwrap();
        assert_eq!(reply["usage"]["input_tokens"], input_tokens);
        assert_eq!(reply["answers"].as_object().unwrap().len(), 3);
        assert_eq!(reply["answers"]["q0"]["noul"], reply["answers"]["q2"]["noul"]);
        // The slot choice is the only new completion field; arithmetic is unchanged.
        let probabilities = super::engine::probabilities(&[0.9_f64.ln(), 0.1_f64.ln()], 0.8);
        assert_eq!(reply["answers"]["q0"]["noul"], json!(super::engine::commit_noul(probabilities[0])));
        assert_eq!(reply["answers"]["q1"]["probabilities"]["first"], json!(probabilities[0]));
    }

    #[tokio::test]
    async fn prefetched_requests_prepare_but_wait_for_distinct_free_slots() {
        let (mut server, engine) = Server::start(2).await;
        let first = answer(&engine, payload("record-a", 2));
        let second = answer(&engine, payload("record-b", 2));
        let a = server.next().await;
        let b = server.next().await;
        let (a_slot, b_slot) = (a.slot(), b.slot());
        assert_ne!(a_slot, b_slot);
        assert!(a_slot < 2 && b_slot < 2);
        let third = answer(&engine, payload("record-c", 2));
        server.prepared(17 + 6).await;
        // Counting is independent of slot ownership too.
        assert!(engine.count(&payload("count-only", 1)).await.unwrap() > 0);
        assert!(server.completions.try_recv().is_err());
        a.finish();
        let a_next = server.next().await;
        assert_eq!(a_next.slot(), a_slot);
        a_next.finish();
        let c = server.next().await;
        assert_eq!(c.slot(), a_slot);
        b.finish();
        let b_next = server.next().await;
        assert_eq!(b_next.slot(), b_slot);
        b_next.finish();
        c.finish();
        let c_next = server.next().await;
        assert_eq!(c_next.slot(), a_slot);
        c_next.finish();
        for job in [first, second, third] {
            job.await.unwrap().unwrap();
        }
    }

    #[tokio::test]
    async fn slot_returns_on_error_cancellation_and_timeout() {
        let (mut server, engine) = Server::start(1).await;
        let job = answer(&engine, payload("failure", 2));
        server.next().await.finish();
        let failed = server.next().await;
        assert_eq!(failed.slot(), 0);
        failed
            .reply
            .send(Err(RuntimeError::new(pool_protocol::messages::JobErrorCode::RuntimeFailed, "fake failure")))
            .unwrap();
        assert!(job.await.unwrap().is_err());

        let cancelled = answer(&engine, payload("cancel", 2));
        let abandoned = server.next().await;
        cancelled.abort();
        assert!(cancelled.await.unwrap_err().is_cancelled());
        assert!(abandoned.reply.is_closed());

        let task_engine = engine.clone();
        let timed = tokio::spawn(async move {
            tokio::time::timeout(Duration::from_millis(100), task_engine.answer(&payload("timeout", 2))).await
        });
        let abandoned = server.next().await;
        assert_eq!(abandoned.slot(), 0);
        assert!(timed.await.unwrap().is_err());
        assert!(abandoned.reply.is_closed());

        let next = answer(&engine, payload("recovered", 1));
        let completion = server.next().await;
        assert_eq!(completion.slot(), 0);
        completion.finish();
        next.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn invalid_slot_count_and_invalid_response_do_not_lose_capacity() {
        let (sender, mut requests) = mpsc::unbounded_channel();
        let result = H2o::start(LocalClient::fake(sender), 8192, 0).await;
        assert!(matches!(result, Err(error) if error.message.contains("at least one")));
        assert!(requests.try_recv().is_err());

        let (mut server, engine) = Server::start(1).await;
        let invalid = answer(&engine, payload("truncated", 1));
        let completion = server.next().await;
        completion.reply.send(Ok(json!({"truncated": true, "tokens_evaluated": 0}))).unwrap();
        assert!(invalid.await.unwrap().unwrap_err().message.contains("truncated"));
        let recovered = answer(&engine, payload("valid", 1));
        let completion = server.next().await;
        assert_eq!(completion.slot(), 0);
        completion.finish();
        recovered.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn cancelling_a_waiter_does_not_lose_a_slot() {
        let (mut server, engine) = Server::start(1).await;
        let first = answer(&engine, payload("first", 1));
        let held = server.next().await;
        let waiting = answer(&engine, payload("waiting", 1));
        server.prepared(19).await;
        waiting.abort();
        assert!(waiting.await.unwrap_err().is_cancelled());
        assert!(server.completions.try_recv().is_err());
        held.finish();
        first.await.unwrap().unwrap();
        let next = answer(&engine, payload("next", 1));
        let completion = server.next().await;
        assert_eq!(completion.slot(), 0);
        completion.finish();
        next.await.unwrap().unwrap();
    }
}
