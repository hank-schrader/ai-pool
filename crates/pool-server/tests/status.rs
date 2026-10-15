//! Public status routes and the unchanged authentication boundaries.

use std::time::Duration;

use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use pool_protocol::{
    Accelerator, Catalog,
    messages::{DeviceSlots, Hardware, LoadedModel, MinerMessage, PoolMessage},
};
use pool_server::{
    AppState,
    config::{AuthMode, Config, MinerAuth},
    router,
};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tower::ServiceExt;

fn config() -> Config {
    Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        catalog: "config/models.json".into(),
        auth_mode: AuthMode::Local,
        client_api_keys: vec![],
        miner_tokens: vec![],
        miner_auth: MinerAuth::Token,
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

#[tokio::test]
async fn completed_questions_reach_the_public_api_without_a_socket() {
    let mut config = config();
    config.heartbeat = Duration::from_secs(120);
    let state = AppState::new(config, Catalog::parse(include_str!("../../../tests/fixtures/all-models.json")).unwrap());
    let scheduler = state.scheduler.clone();
    let model = state.catalog.model("clef").unwrap();
    let (outbound, mut messages) = mpsc::channel(16);
    scheduler.miner_connected(
        "fake",
        "fake",
        "fake",
        Hardware { os: "test".into(), arch: "test".into(), accelerator: Accelerator::Cuda, devices: vec![] },
        outbound,
    );
    scheduler.miner_message(
        "fake",
        MinerMessage::Capacity {
            devices: vec![DeviceSlots { id: "gpu0".into(), slots: 1 }],
            models: vec![LoadedModel {
                model: model.id.clone(),
                model_revision: state.catalog.model_revision(model),
                profile: "cuda-4096".into(),
                device: "gpu0".into(),
                context_tokens: 4096,
            }],
        },
    );
    let miner = scheduler.clone();
    tokio::spawn(async move {
        while let Some(message) = messages.recv().await {
            let reply = match message {
                PoolMessage::CountTokens { count_id, .. } => MinerMessage::CountResult { count_id, input_tokens: 100 },
                PoolMessage::Job { attempt_id, payload, .. } => {
                    let answers: serde_json::Map<String, Value> = payload["questions"]
                        .as_object()
                        .unwrap()
                        .keys()
                        .map(|id| (id.clone(), json!({"type": "noul", "noul": 0.9})))
                        .collect();
                    MinerMessage::Result { attempt_id, body: json!({"answers": answers}) }
                }
                _ => continue,
            };
            miner.miner_message("fake", reply);
        }
    });
    let app = router(state);
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(61)).await;
    scheduler.snapshot().await;
    tokio::time::resume();
    let request = json!({"model": "clef", "state": "short", "questions": {
        "q": {"type": "noul", "instructions": "one"},
        "r": {"type": "noul", "instructions": "two"},
        "s": {"type": "noul", "instructions": "three"}
    }});
    let response = app
        .clone()
        .oneshot(Request::builder().method("POST").uri("/v1/systemone").body(Body::from(request.to_string())).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = serde_json::from_slice(&to_bytes(response.into_body(), 1 << 20).await.unwrap()).unwrap();
    assert_eq!(body["answers"].as_object().unwrap().len(), 3);
    assert_eq!(scheduler.stats().answered_per_second, 3.0 / 60.0);
    assert_eq!(scheduler.stats().average_prompt_tokens, Some(100.0));
    assert_eq!(scheduler.stats().answering_questions, 0);
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(61)).await;
    scheduler.snapshot().await;
    tokio::time::advance(Duration::from_secs(1)).await;
    tokio::task::yield_now().await;
    tokio::time::resume();
    let response = app.oneshot(Request::builder().uri("/v1/stats").body(Body::empty()).unwrap()).await.unwrap();
    let stats: Value = serde_json::from_slice(&to_bytes(response.into_body(), 1 << 20).await.unwrap()).unwrap();
    assert_eq!(stats["answered_last_24h"], 3);
    assert_eq!(stats["answered_last_hour"], 3);
    assert_eq!(stats["answering_questions"], 0);
    assert_eq!(stats["average_prompt_tokens"], Value::Null);
    assert_eq!(stats["history"]["requests"], json!([1]));
    assert_eq!(stats["history"]["prompt_tokens"], json!([100]));
    assert_eq!(stats["answered_per_second"], 0.0);
    assert_eq!(stats["queued_questions"], 0);
    assert_eq!(stats["miners"], 1);
    assert_eq!(stats["history"]["answered"].as_array().unwrap().iter().map(|n| n.as_u64().unwrap()).sum::<u64>(), 3);
    println!("/v1/stats completed-job sample: {stats}");
}

#[tokio::test]
async fn public_status_routes_need_no_auth_and_set_security_and_cache_headers() {
    let mut config = config();
    config.auth_mode = AuthMode::Keys;
    config.client_api_keys = vec!["client-secret".into()];
    config.miner_tokens = vec!["miner-secret".into()];
    config.admin_token = Some("admin-secret".into());
    let app =
        router(AppState::new(config, Catalog::parse(include_str!("../../../tests/fixtures/all-models.json")).unwrap()));

    let response = app.clone().oneshot(Request::builder().uri("/").body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "text/html; charset=utf-8");
    assert_eq!(
        response.headers()["content-security-policy"],
        "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self' data:; base-uri 'none'; frame-ancestors 'none'"
    );
    let page = String::from_utf8(to_bytes(response.into_body(), 1 << 20).await.unwrap().to_vec()).unwrap();
    assert!(page.contains("./assets/"));
    assert!(!page.contains("<style"));
    assert!(!page.contains("style="));
    assert!(!page.contains("mockStats"));

    for path in ["/healthz", "/readyz"] {
        let response = app.clone().oneshot(Request::builder().uri(path).body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{path}");
    }

    let response = app.clone().oneshot(Request::builder().uri("/v1/stats").body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["cache-control"], "public, max-age=5");
    assert_eq!(response.headers()["content-type"], "application/json");
    let stats: Value = serde_json::from_slice(&to_bytes(response.into_body(), 1 << 20).await.unwrap()).unwrap();
    println!("/v1/stats sample: {stats}");
    assert_eq!(stats["miners"], 0);
    assert_eq!(stats["answering_questions"], 0);
    assert_eq!(stats["average_prompt_tokens"], Value::Null);
    assert_eq!(stats["queued_questions"], 0);
    assert_eq!(stats["answered_per_second"], 0.0);
    assert_eq!(stats["answered_last_24h"], 0);
    assert_eq!(stats["answered_last_hour"], 0);
    assert_eq!(stats["history"]["interval_seconds"], 60);
    for field in ["answered", "queued", "miners", "answering", "requests", "prompt_tokens"] {
        assert_eq!(stats["history"][field], serde_json::json!([]));
    }

    for (method, path) in [
        ("GET", "/v1/models"),
        ("GET", "/v1/models/clef"),
        ("POST", "/v1/systemone"),
        ("POST", "/v1/chat/completions"),
        ("GET", "/admin/v1/status"),
        ("GET", "/miner/v1/catalog"),
    ] {
        let response = app
            .clone()
            .oneshot(Request::builder().method(method).uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{method} {path}");
    }
}
