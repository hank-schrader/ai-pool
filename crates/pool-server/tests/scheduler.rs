//! End-to-end scheduler behaviour against scripted fake miners.
//!
//! Fake miners count tokens from a `tokens:N` marker in the request text, so
//! each test controls exactly how much context a request needs.

use std::{net::SocketAddr, time::Duration};

use futures_util::{SinkExt, StreamExt};
use pool_protocol::{
    Accelerator, Catalog,
    messages::{Device, DeviceSlots, Hardware, LoadedModel, MinerMessage, PROTOCOL_VERSION, PoolMessage, SUBPROTOCOL},
};
use pool_server::{
    AppState,
    config::{AuthMode, Config, MinerAuth},
    router,
};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

const CATALOG: &str = include_str!("../../../config/models.json");

fn config() -> Config {
    Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        catalog: "config/models.json".into(),
        auth_mode: AuthMode::Local,
        client_api_keys: vec![],
        miner_tokens: vec![],
        miner_auth: MinerAuth::Token,
        max_miners: 256,
        admin_token: None,
        request_timeout: Duration::from_secs(20),
        count_timeout: Duration::from_secs(5),
        queue_timeout: Duration::from_secs(5),
        dispatch_ack_timeout: Duration::from_secs(3),
        first_chunk_timeout: Duration::from_secs(10),
        queue_capacity_global: 64,
        queue_capacity_per_model: 32,
        max_active_per_client: 4,
        max_queued_per_client: 8,
        heartbeat: Duration::from_secs(10),
        stream_buffer_events: 64,
    }
}

async fn start(config: Config) -> SocketAddr {
    let state = AppState::new(config, Catalog::parse(CATALOG).unwrap());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router(state)).await.unwrap() });
    addr
}

#[derive(Clone, Copy, Debug)]
enum Behavior {
    /// Answer every job.
    Answer,
    /// Accept a job, then drop the connection.
    DropAfterAccept,
    /// Stream `chunks` content chunks, then either finish or drop the connection.
    Stream { chunks: u64, then_drop: bool },
    /// Stream one chunk, then wait (for cancellation tests).
    StreamAndHang,
    /// Fail every job the way a broken runtime does.
    FailRuntime,
}

#[derive(Debug)]
struct Seen {
    miner: &'static str,
    message: PoolMessage,
}

fn requested_tokens(payload: &Value) -> u32 {
    let text = payload["state"]
        .as_str()
        .map(str::to_owned)
        .or_else(|| payload["messages"][0]["content"].as_str().map(str::to_owned))
        .unwrap_or_default();
    text.strip_prefix("tokens:").and_then(|n| n.split_whitespace().next()?.parse().ok()).unwrap_or(10)
}

/// Connects a fake miner serving `models` as (model, profile) and runs `behavior`.
async fn fake_miner(
    addr: SocketAddr,
    name: &'static str,
    token: Option<&str>,
    models: &[(&str, &str)],
    behavior: Behavior,
    seen: mpsc::UnboundedSender<Seen>,
) {
    let catalog = Catalog::parse(CATALOG).unwrap();
    let mut request = format!("ws://{addr}/miner/v1/connect").into_client_request().unwrap();
    request.headers_mut().insert("sec-websocket-protocol", SUBPROTOCOL.parse().unwrap());
    if let Some(token) = token {
        request.headers_mut().insert("authorization", format!("Bearer {token}").parse().unwrap());
    }
    let (socket, _) = tokio_tungstenite::connect_async(request).await.expect("miner connects");
    let (mut sink, mut stream) = socket.split();
    let send = |message: MinerMessage| Message::Text(serde_json::to_string(&message).unwrap().into());

    sink.send(send(MinerMessage::Hello {
        protocol_version: PROTOCOL_VERSION,
        miner_id: name.into(),
        miner_version: "test".into(),
        catalog_revision: catalog.revision(),
        hardware: Hardware {
            os: "linux".into(),
            arch: "x86_64".into(),
            accelerator: Accelerator::Cuda,
            devices: vec![Device { id: "gpu0".into(), name: "Fake GPU".into(), memory_total_mib: 16384 }],
        },
    }))
    .await
    .unwrap();
    let welcome = stream.next().await.unwrap().unwrap();
    assert!(welcome.to_text().unwrap().contains("welcome"), "{welcome:?}");

    let loaded = models
        .iter()
        .map(|(model_id, profile)| {
            let model = catalog.model(model_id).unwrap();
            LoadedModel {
                model: model_id.to_string(),
                model_revision: catalog.model_revision(model),
                profile: profile.to_string(),
                device: "gpu0".into(),
                context_tokens: model.profile(profile).unwrap().context_tokens,
            }
        })
        .collect();
    sink.send(send(MinerMessage::Capacity {
        devices: vec![DeviceSlots { id: "gpu0".into(), slots: 1 }],
        models: loaded,
    }))
    .await
    .unwrap();

    tokio::spawn(async move {
        while let Some(Ok(frame)) = stream.next().await {
            let Message::Text(text) = frame else { continue };
            let message: PoolMessage = serde_json::from_str(&text).unwrap();
            let _ = seen.send(Seen { miner: name, message: message.clone() });
            match message {
                PoolMessage::CountTokens { count_id, payload, .. } => {
                    let input_tokens = requested_tokens(&payload);
                    sink.send(send(MinerMessage::CountResult { count_id, input_tokens })).await.unwrap();
                }
                PoolMessage::Job { attempt_id, operation, .. } => {
                    sink.send(send(MinerMessage::Accepted { attempt_id: attempt_id.clone() })).await.unwrap();
                    match behavior {
                        Behavior::DropAfterAccept => return,
                        Behavior::FailRuntime => {
                            let message = "runtime returned HTTP 500: Compute error.".to_string();
                            let code = pool_protocol::messages::JobErrorCode::RuntimeFailed;
                            sink.send(send(MinerMessage::JobError { attempt_id, code, message })).await.unwrap();
                        }
                        Behavior::Answer => {
                            let body = match operation {
                                pool_protocol::Operation::Systemone => json!({
                                    "model": "Clef-Flash-Q8_0.gguf",
                                    "answers": {"q": {"type": "noul", "noul": 0.9}},
                                    "usage": {"input_tokens": 120, "output_tokens": 0}
                                }),
                                pool_protocol::Operation::ChatCompletions => json!({
                                    "id": "chatcmpl-runtime", "object": "chat.completion", "created": 1,
                                    "model": "qwen.gguf", "timings": {},
                                    "choices": [{"index": 0, "message": {"role": "assistant", "content": "hi"}, "finish_reason": "stop"}],
                                    "usage": {"prompt_tokens": 10, "completion_tokens": 1, "total_tokens": 11}
                                }),
                            };
                            sink.send(send(MinerMessage::Result { attempt_id, body })).await.unwrap();
                        }
                        Behavior::Stream { chunks, then_drop } => {
                            sink.send(send(MinerMessage::StreamStart { attempt_id: attempt_id.clone() }))
                                .await
                                .unwrap();
                            for seq in 0..chunks {
                                let chunk = json!({
                                    "id": "chatcmpl-runtime", "object": "chat.completion.chunk", "created": 1, "model": "qwen.gguf",
                                    "choices": [{"index": 0, "delta": {"content": format!("w{seq}")}, "finish_reason": null}]
                                });
                                sink.send(send(MinerMessage::StreamChunk {
                                    attempt_id: attempt_id.clone(),
                                    seq,
                                    chunk,
                                }))
                                .await
                                .unwrap();
                            }
                            if then_drop {
                                return;
                            }
                            let usage =
                                json!({"prompt_tokens": 10, "completion_tokens": chunks, "total_tokens": 10 + chunks});
                            sink.send(send(MinerMessage::StreamEnd { attempt_id, usage: Some(usage) })).await.unwrap();
                        }
                        Behavior::StreamAndHang => {
                            let chunk = json!({"object": "chat.completion.chunk", "choices": [{"index": 0, "delta": {"content": "w"}}]});
                            sink.send(send(MinerMessage::StreamChunk { attempt_id, seq: 0, chunk })).await.unwrap();
                        }
                    }
                }
                PoolMessage::Cancel { attempt_id } => {
                    sink.send(send(MinerMessage::Cancelled { attempt_id })).await.unwrap();
                }
                _ => {}
            }
        }
    });
}

async fn wait_ready(addr: SocketAddr, model: &str, miners: usize) {
    for _ in 0..100 {
        let models: Value = reqwest::get(format!("http://{addr}/v1/models")).await.unwrap().json().await.unwrap();
        let entry = models["data"].as_array().unwrap().iter().find(|m| m["id"] == model).unwrap();
        if entry["pool"]["ready_miners"].as_u64().unwrap() as usize >= miners {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("miners for {model} never became ready");
}

fn clef_request(tokens: u32) -> Value {
    json!({"model": "clef", "state": format!("tokens:{tokens}"), "questions": {"q": {"type": "noul", "instructions": "x"}}})
}

fn chat_request(tokens: u32, stream: bool) -> Value {
    json!({
        "model": "qwen2.5-1.5b-instruct",
        "messages": [{"role": "user", "content": format!("tokens:{tokens}")}],
        "max_tokens": 64,
        "stream": stream,
        "stream_options": if stream { json!({"include_usage": true}) } else { Value::Null },
    })
}

async fn post(addr: SocketAddr, path: &str, body: &Value) -> reqwest::Response {
    reqwest::Client::new().post(format!("http://{addr}{path}")).json(body).send().await.unwrap()
}

fn jobs(seen: &mut mpsc::UnboundedReceiver<Seen>) -> Vec<(&'static str, String)> {
    let mut jobs = Vec::new();
    while let Ok(seen) = seen.try_recv() {
        if let PoolMessage::Job { profile, .. } = seen.message {
            jobs.push((seen.miner, profile));
        }
    }
    jobs
}

/// Parses an SSE body into its `data:` payloads.
fn sse_data(body: &str) -> Vec<String> {
    body.lines().filter_map(|line| line.strip_prefix("data: ").map(str::to_owned)).collect()
}

#[tokio::test]
async fn systemone_round_trip_rewrites_model_name() {
    let addr = start(config()).await;
    let (tx, mut seen) = mpsc::unbounded_channel();
    fake_miner(addr, "a", None, &[("clef", "cuda-4096")], Behavior::Answer, tx).await;
    wait_ready(addr, "clef", 1).await;

    let response = post(addr, "/v1/systemone", &clef_request(120)).await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-model-profile"], "cuda-4096");
    assert!(response.headers()["x-request-id"].to_str().unwrap().starts_with("req_"));
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["model"], "clef");
    assert_eq!(body["answers"]["q"]["noul"], 0.9);
    assert_eq!(jobs(&mut seen), vec![("a", "cuda-4096".to_string())]);
}

#[tokio::test]
async fn routes_to_the_smallest_profile_that_fits() {
    let addr = start(config()).await;
    let (tx, mut seen) = mpsc::unbounded_channel();
    fake_miner(addr, "small", None, &[("clef", "cuda-4096")], Behavior::Answer, tx.clone()).await;
    fake_miner(addr, "large", None, &[("clef", "cuda-16384")], Behavior::Answer, tx).await;
    wait_ready(addr, "clef", 2).await;

    assert_eq!(post(addr, "/v1/systemone", &clef_request(5000)).await.status(), 200);
    assert_eq!(post(addr, "/v1/systemone", &clef_request(100)).await.status(), 200);
    assert_eq!(jobs(&mut seen), vec![("large", "cuda-16384".into()), ("small", "cuda-4096".into())]);

    // larger than every catalog profile: rejected without dispatching
    let response = post(addr, "/v1/systemone", &clef_request(20000)).await;
    assert_eq!(response.status(), 400);
    assert_eq!(response.json::<Value>().await.unwrap()["error"]["code"], "context_length_exceeded");
    assert!(jobs(&mut seen).is_empty());
}

#[tokio::test]
async fn context_unavailable_when_only_small_profiles_are_loaded() {
    let addr = start(config()).await;
    let (tx, _seen) = mpsc::unbounded_channel();
    fake_miner(addr, "small", None, &[("clef", "cuda-4096")], Behavior::Answer, tx).await;
    wait_ready(addr, "clef", 1).await;
    let response = post(addr, "/v1/systemone", &clef_request(5000)).await;
    assert_eq!(response.status(), 503);
    assert_eq!(response.json::<Value>().await.unwrap()["error"]["code"], "context_unavailable");
}

#[tokio::test]
async fn retries_on_another_miner_before_anything_is_committed() {
    let addr = start(config()).await;
    let (tx, mut seen) = mpsc::unbounded_channel();
    // the flaky miner has the smaller profile, so it is tried first
    fake_miner(addr, "flaky", None, &[("clef", "cuda-4096")], Behavior::DropAfterAccept, tx.clone()).await;
    fake_miner(addr, "steady", None, &[("clef", "cuda-8192")], Behavior::Answer, tx).await;
    wait_ready(addr, "clef", 2).await;

    let response = post(addr, "/v1/systemone", &clef_request(100)).await;
    assert_eq!(response.status(), 200);
    assert_eq!(jobs(&mut seen), vec![("flaky", "cuda-4096".into()), ("steady", "cuda-8192".into())]);
}

#[tokio::test]
async fn streams_chunks_usage_and_done() {
    let addr = start(config()).await;
    let (tx, _seen) = mpsc::unbounded_channel();
    let behavior = Behavior::Stream { chunks: 3, then_drop: false };
    fake_miner(addr, "a", None, &[("qwen2.5-1.5b-instruct", "cuda-4096")], behavior, tx).await;
    wait_ready(addr, "qwen2.5-1.5b-instruct", 1).await;

    let response = post(addr, "/v1/chat/completions", &chat_request(10, true)).await;
    assert_eq!(response.status(), 200);
    let request_id = response.headers()["x-request-id"].to_str().unwrap().to_owned();
    let data = sse_data(&response.text().await.unwrap());
    assert_eq!(data.len(), 5, "{data:?}");
    assert_eq!(data[4], "[DONE]");
    let chunks: Vec<Value> = data[..4].iter().map(|d| serde_json::from_str(d).unwrap()).collect();
    let expected_id = format!("chatcmpl-{}", request_id.trim_start_matches("req_"));
    assert!(chunks.iter().all(|c| c["id"] == expected_id && c["model"] == "qwen2.5-1.5b-instruct"));
    assert_eq!(chunks[2]["choices"][0]["delta"]["content"], "w2");
    assert_eq!(chunks[3]["choices"], json!([]));
    assert_eq!(chunks[3]["usage"]["completion_tokens"], 3);
}

#[tokio::test]
async fn never_replays_a_stream_after_the_first_chunk() {
    let addr = start(config()).await;
    let (tx, mut seen) = mpsc::unbounded_channel();
    let flaky = Behavior::Stream { chunks: 2, then_drop: true };
    fake_miner(addr, "flaky", None, &[("qwen2.5-1.5b-instruct", "cuda-4096")], flaky, tx.clone()).await;
    let steady = Behavior::Stream { chunks: 2, then_drop: false };
    fake_miner(addr, "steady", None, &[("qwen2.5-1.5b-instruct", "cuda-8192")], steady, tx).await;
    wait_ready(addr, "qwen2.5-1.5b-instruct", 2).await;

    let response = post(addr, "/v1/chat/completions", &chat_request(10, true)).await;
    assert_eq!(response.status(), 200);
    let data = sse_data(&response.text().await.unwrap());
    assert_eq!(data.len(), 3, "{data:?}");
    assert!(data[2].contains("miner_failed"), "{data:?}");
    assert!(!data.iter().any(|d| d == "[DONE]"));
    assert_eq!(jobs(&mut seen), vec![("flaky", "cuda-4096".into())]);
}

#[tokio::test]
async fn client_disconnect_cancels_the_attempt() {
    let addr = start(config()).await;
    let (tx, mut seen) = mpsc::unbounded_channel();
    fake_miner(addr, "a", None, &[("qwen2.5-1.5b-instruct", "cuda-4096")], Behavior::StreamAndHang, tx).await;
    wait_ready(addr, "qwen2.5-1.5b-instruct", 1).await;

    let mut response = post(addr, "/v1/chat/completions", &chat_request(10, true)).await;
    assert_eq!(response.status(), 200);
    let first = response.chunk().await.unwrap().unwrap();
    assert!(String::from_utf8_lossy(&first).contains("\"w\""));
    drop(response);

    let cancelled = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(seen) = seen.recv().await {
            if let PoolMessage::Cancel { .. } = seen.message {
                return true;
            }
        }
        false
    })
    .await;
    assert_eq!(cancelled, Ok(true));

    // the slot is free again once the miner confirmed
    tokio::time::sleep(Duration::from_millis(100)).await;
    let models: Value = reqwest::get(format!("http://{addr}/v1/models")).await.unwrap().json().await.unwrap();
    let qwen = models["data"].as_array().unwrap().iter().find(|m| m["id"] == "qwen2.5-1.5b-instruct").unwrap();
    assert_eq!(qwen["pool"]["free_slots"], 1);
}

#[tokio::test]
async fn non_streaming_chat_gets_a_pool_completion_id() {
    let addr = start(config()).await;
    let (tx, _seen) = mpsc::unbounded_channel();
    fake_miner(addr, "a", None, &[("qwen2.5-1.5b-instruct", "cuda-4096")], Behavior::Answer, tx).await;
    wait_ready(addr, "qwen2.5-1.5b-instruct", 1).await;
    let body: Value = post(addr, "/v1/chat/completions", &chat_request(10, false)).await.json().await.unwrap();
    assert!(body["id"].as_str().unwrap().starts_with("chatcmpl-"));
    assert_ne!(body["id"], "chatcmpl-runtime");
    assert_eq!(body["model"], "qwen2.5-1.5b-instruct");
    assert!(body.get("timings").is_none());
}

#[tokio::test]
async fn keys_mode_requires_credentials() {
    let mut config = config();
    config.auth_mode = AuthMode::Keys;
    config.client_api_keys = vec!["client-secret".into()];
    config.miner_tokens = vec!["miner-secret".into()];
    let addr = start(config).await;

    let client = reqwest::Client::new();
    let url = format!("http://{addr}/v1/models");
    assert_eq!(client.get(&url).send().await.unwrap().status(), 401);
    assert_eq!(client.get(&url).bearer_auth("wrong").send().await.unwrap().status(), 401);
    assert_eq!(client.get(&url).bearer_auth("client-secret").send().await.unwrap().status(), 200);

    let mut request = format!("ws://{addr}/miner/v1/connect").into_client_request().unwrap();
    request.headers_mut().insert("authorization", "Bearer client-secret".parse().unwrap());
    assert!(tokio_tungstenite::connect_async(request).await.is_err(), "client keys cannot register miners");

    let (tx, _seen) = mpsc::unbounded_channel();
    fake_miner(addr, "a", Some("miner-secret"), &[("clef", "cuda-4096")], Behavior::Answer, tx).await;
    let response = client
        .post(format!("http://{addr}/v1/systemone"))
        .bearer_auth("client-secret")
        .json(&clef_request(10))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
}

#[tokio::test]
async fn fails_fast_when_no_other_miner_can_retry() {
    let addr = start(config()).await;
    let (tx, mut seen) = mpsc::unbounded_channel();
    fake_miner(addr, "broken", None, &[("clef", "cuda-8192")], Behavior::FailRuntime, tx).await;
    wait_ready(addr, "clef", 1).await;

    let started = std::time::Instant::now();
    let response = post(addr, "/v1/systemone", &clef_request(100)).await;
    assert_eq!(response.status(), 502);
    let error = response.json::<Value>().await.unwrap()["error"].clone();
    assert_eq!(error["code"], "miner_failed");
    assert!(error["message"].as_str().unwrap().contains("Compute error"), "{error}");
    // well inside the 5 s queue timeout of the test config
    assert!(started.elapsed() < Duration::from_secs(2), "took {:?}", started.elapsed());
    assert_eq!(jobs(&mut seen).len(), 1);
}

#[tokio::test]
async fn open_pools_take_anonymous_miners_but_still_need_client_keys() {
    let mut config = config();
    config.auth_mode = AuthMode::Keys;
    config.client_api_keys = vec!["client-secret".into()];
    config.miner_auth = MinerAuth::Open;
    let addr = start(config).await;

    let (tx, _seen) = mpsc::unbounded_channel();
    fake_miner(addr, "anon", None, &[("clef", "cuda-4096")], Behavior::Answer, tx).await;
    let client = reqwest::Client::new();
    let url = format!("http://{addr}/v1/systemone");
    assert_eq!(client.post(&url).json(&clef_request(10)).send().await.unwrap().status(), 401);
    let response = client.post(&url).bearer_auth("client-secret").json(&clef_request(10)).send().await.unwrap();
    assert_eq!(response.status(), 200);
}

#[tokio::test]
async fn refuses_miners_beyond_the_connection_limit() {
    let mut config = config();
    config.max_miners = 1;
    let addr = start(config).await;
    let (tx, _seen) = mpsc::unbounded_channel();
    fake_miner(addr, "first", None, &[("clef", "cuda-4096")], Behavior::Answer, tx).await;
    let mut request = format!("ws://{addr}/miner/v1/connect").into_client_request().unwrap();
    request.headers_mut().insert("sec-websocket-protocol", SUBPROTOCOL.parse().unwrap());
    let refused = tokio_tungstenite::connect_async(request).await.unwrap_err().to_string();
    assert!(refused.contains("503"), "{refused}");
}

#[tokio::test]
async fn offload_profiles_are_only_sent_to_miners_that_ask() {
    let addr = start(config()).await;
    let count_offload = |reply: Value| {
        reply["catalog"]["models"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|model| model["profiles"].as_array().unwrap().clone())
            .filter(|profile| profile.get("gpu_layers").is_some())
            .count()
    };
    let legacy: Value = reqwest::get(format!("http://{addr}/miner/v1/catalog")).await.unwrap().json().await.unwrap();
    assert_eq!(count_offload(legacy.clone()), 0);
    // the reduced view must still be a valid catalog for old miners, with its own revision
    let parsed = Catalog::parse(&legacy["catalog"].to_string()).unwrap();
    assert_eq!(parsed.revision(), legacy["catalog_revision"].as_str().unwrap());

    let current: Value =
        reqwest::get(format!("http://{addr}/miner/v1/catalog?features=offload")).await.unwrap().json().await.unwrap();
    assert!(count_offload(current) > 0);
}
