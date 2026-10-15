//! Bounded statistics publication, reconnects, lag recovery and embedded assets.
use std::{sync::Arc, time::Duration};

use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
    response::Response,
};
use futures_util::{StreamExt, stream::Stream};
use pool_protocol::Catalog;
use pool_server::{
    AppState, assets,
    config::{AuthMode, Config, MinerAuth},
    router,
    stats::{History, StatsPoint, StatsSnapshot},
    stats_stream::StatsPublisher,
};
use serde_json::Value;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::sync::watch;
use tower::ServiceExt;

fn timestamp(second: i64) -> String {
    OffsetDateTime::from_unix_timestamp(second).unwrap().format(&Rfc3339).unwrap()
}
fn snapshot(second: i64) -> StatsSnapshot {
    StatsSnapshot {
        updated_at: timestamp(second),
        miners: 2,
        queued_questions: 3,
        answering_questions: 4,
        average_prompt_tokens: Some(100.0),
        answered_per_second: 0.5,
        answered_last_24h: 60,
        answered_last_hour: 61,
        live: Arc::new(
            (180..second)
                .map(|s| StatsPoint {
                    t: timestamp(s),
                    answered: 1,
                    queued: 3,
                    miners: 2,
                    answering: 4,
                    requests: 1,
                    prompt_tokens: 100,
                })
                .collect(),
        ),
        history: Arc::new(History {
            interval_seconds: 60,
            end: timestamp(120),
            answered: vec![60],
            queued: vec![3],
            miners: vec![2],
            answering: vec![4],
            requests: vec![60],
            prompt_tokens: vec![6000],
        }),
    }
}
async fn publish(sender: &watch::Sender<StatsSnapshot>, second: i64) {
    sender.send_replace(snapshot(second));
    tokio::time::advance(Duration::from_secs(1)).await;
    tokio::task::yield_now().await;
}
async fn event<S>(body: &mut S) -> (String, Value)
where
    S: Stream<Item = Result<bytes::Bytes, axum::Error>> + Unpin,
{
    let bytes = body.next().await.unwrap().unwrap();
    let text = std::str::from_utf8(&bytes).unwrap();
    assert!(!text.lines().any(|line| line.starts_with("id:")));
    let name = text.lines().find_map(|line| line.strip_prefix("event: ")).unwrap().to_owned();
    let data = text.lines().find_map(|line| line.strip_prefix("data: ")).unwrap();
    (name, serde_json::from_str(data).unwrap())
}

#[tokio::test(start_paused = true)]
async fn snapshots_ticks_reconnects_lag_and_skipped_seconds_are_consistent() {
    let (sender, receiver) = watch::channel(snapshot(181));
    let publisher = StatsPublisher::spawn(receiver, 2);
    tokio::task::yield_now().await;
    let response = publisher.response();
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    assert_eq!(response.headers()["cache-control"], "no-store, no-transform");
    let mut body = response.into_body().into_data_stream();
    let (name, initial) = event(&mut body).await;
    assert_eq!(name, "snapshot");
    assert_eq!(initial["version"], 1);
    assert!(uuid::Uuid::parse_str(initial["epoch"].as_str().unwrap()).is_ok());
    assert_eq!(initial["seq"], 0);
    assert_eq!(initial["totals"]["miners"], 2);
    assert_eq!(initial["totals"]["answered_last_hour"], 61);
    assert_eq!(initial["totals"]["answering_questions"], 4);
    assert_eq!(initial["totals"]["average_prompt_tokens"], 100.0);
    assert_eq!(initial["live"][0]["t"], timestamp(180));
    publish(&sender, 182).await;
    let (name, tick) = event(&mut body).await;
    assert_eq!(name, "tick");
    assert_eq!(tick["seq"], 1);
    assert_eq!(tick["totals"]["answered_last_hour"], 61);
    assert_eq!(tick["second"]["t"], timestamp(181));
    assert_eq!(tick["second"]["answering"], 4);
    assert_eq!(tick["second"]["requests"], 1);
    assert_eq!(tick["second"]["prompt_tokens"], 100);
    assert_eq!(tick["minute"], Value::Null);
    assert_eq!(tick["epoch"], initial["epoch"]);
    let mut reconnect = publisher.response().into_body().into_data_stream();
    let (name, replacement) = event(&mut reconnect).await;
    assert_eq!(name, "snapshot");
    assert_eq!(replacement["seq"], tick["seq"]);
    assert_eq!(replacement["live"][1], tick["second"]);
    drop(reconnect);
    publish(&sender, 183).await;
    publish(&sender, 184).await;
    let (name, lagged) = event(&mut body).await;
    assert_eq!(name, "snapshot");
    assert_eq!(lagged["seq"], 3);
    assert_eq!(lagged["live"].as_array().unwrap().len(), 4);
    publish(&sender, 190).await;
    let (name, skipped) = event(&mut body).await;
    assert_eq!(name, "snapshot");
    assert_eq!(skipped["seq"], 4);
    assert_eq!(skipped["live"].as_array().unwrap().len(), 10);
    publish(&sender, 191).await;
    assert_eq!(event(&mut body).await.0, "tick");
    // Queue/miner changes within a second are not new shared publications.
    let mut same = snapshot(191);
    same.miners = 8;
    sender.send_replace(same);
    tokio::time::advance(Duration::from_secs(1)).await;
    tokio::task::yield_now().await;
    assert_eq!(publisher.latest().snapshot.shared.seq, 5);
    drop(sender);
    tokio::task::yield_now().await;
    assert!(body.next().await.is_none());
}

#[tokio::test(start_paused = true)]
async fn first_completed_second_is_a_tick_after_an_empty_live_snapshot() {
    let (sender, receiver) = watch::channel(snapshot(180));
    let publisher = StatsPublisher::spawn(receiver, 1);
    tokio::task::yield_now().await;
    let mut body = publisher.response().into_body().into_data_stream();
    assert_eq!(event(&mut body).await.0, "snapshot");
    publish(&sender, 181).await;
    let (name, tick) = event(&mut body).await;
    assert_eq!(name, "tick");
    assert_eq!(tick["seq"], 1);
    assert_eq!(tick["totals"]["answered_last_hour"], 61);
    assert_eq!(tick["second"]["t"], timestamp(180));
    assert!(tick["minute"].is_null());
}

#[tokio::test(start_paused = true)]
async fn completed_minute_and_rest_are_from_the_same_shared_frame() {
    let (sender, receiver) = watch::channel(snapshot(239));
    let publisher = StatsPublisher::spawn(receiver, 1);
    tokio::task::yield_now().await;
    let mut body = publisher.response().into_body().into_data_stream();
    event(&mut body).await;
    let mut complete = snapshot(240);
    complete.history = Arc::new(History {
        interval_seconds: 60,
        end: timestamp(180),
        answered: vec![60, 60],
        queued: vec![3, 4],
        miners: vec![2, 1],
        answering: vec![4, 5],
        requests: vec![60, 60],
        prompt_tokens: vec![6000, 6000],
    });
    sender.send_replace(complete);
    tokio::time::advance(Duration::from_secs(1)).await;
    tokio::task::yield_now().await;
    let (name, tick) = event(&mut body).await;
    assert_eq!(name, "tick");
    assert_eq!(tick["minute"]["t"], timestamp(180));
    assert_eq!(tick["minute"]["answered"], 60);
    assert_eq!(tick["minute"]["answering"], 5);
    assert_eq!(tick["minute"]["requests"], 60);
    assert_eq!(tick["minute"]["prompt_tokens"], 6000);
    let frame = publisher.latest();
    let rest: Value = serde_json::from_str(&frame.rest_json).unwrap();
    let full: Value = serde_json::from_str(&frame.snapshot_json).unwrap();
    assert_eq!(rest["seq"], tick["seq"]);
    assert_eq!(rest["epoch"], tick["epoch"]);
    assert_eq!(rest["history"], full["history"]);
    assert_eq!(rest["miners"], tick["totals"]["miners"]);
    assert_eq!(rest["answering_questions"], tick["totals"]["answering_questions"]);
    assert_eq!(rest["average_prompt_tokens"], tick["totals"]["average_prompt_tokens"]);
    assert_eq!(full["live"].as_array().unwrap().last().unwrap(), &tick["second"]);
}

#[tokio::test(start_paused = true)]
async fn capacity_is_immediate_and_permit_is_held_until_body_drop() {
    let (_sender, receiver) = watch::channel(snapshot(181));
    let publisher = StatsPublisher::spawn(receiver, 1);
    let held = publisher.response();
    let refused = publisher.response();
    assert_eq!(refused.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(refused.headers()["retry-after"], "30");
    assert_eq!(refused.headers()["cache-control"], "no-store");
    let mut body = held.into_body().into_data_stream();
    event(&mut body).await;
    assert_eq!(publisher.response().status(), StatusCode::SERVICE_UNAVAILABLE);
    drop(body);
    assert_eq!(publisher.response().status(), StatusCode::OK);
}

#[tokio::test(start_paused = true)]
async fn keep_alive_is_a_comment_after_fifteen_seconds() {
    let (_sender, receiver) = watch::channel(snapshot(181));
    let publisher = StatsPublisher::spawn(receiver, 1);
    let mut body = publisher.response().into_body().into_data_stream();
    event(&mut body).await;
    tokio::time::advance(Duration::from_secs(15)).await;
    let bytes = body.next().await.unwrap().unwrap();
    assert!(std::str::from_utf8(&bytes).unwrap().starts_with(':'));
}

fn config() -> Config {
    Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        catalog: "config/models.json".into(),
        auth_mode: AuthMode::Local,
        client_api_keys: vec![],
        miner_tokens: vec![],
        miner_auth: MinerAuth::Token,
        max_miners: 256,
        max_stats_streams: 1,
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
async fn get(app: &axum::Router, path: &str) -> Response {
    app.clone().oneshot(Request::builder().uri(path).body(Body::empty()).unwrap()).await.unwrap()
}

#[tokio::test]
async fn assets_have_exact_lookup_mime_csp_and_cache_headers() {
    let app = router(AppState::new(
        config(),
        Catalog::parse(include_str!("../../../tests/fixtures/all-models.json")).unwrap(),
    ));
    let response = get(&app, "/").await;
    assert_eq!(response.headers()["content-type"], "text/html; charset=utf-8");
    assert_eq!(response.headers()["cache-control"], "no-cache");
    assert_eq!(response.headers()["x-content-type-options"], "nosniff");
    assert_eq!(response.headers()["content-security-policy"], assets::CSP);
    let html = String::from_utf8(to_bytes(response.into_body(), 1 << 20).await.unwrap().to_vec()).unwrap();
    for (extension, mime) in [("js", "text/javascript; charset=utf-8"), ("css", "text/css; charset=utf-8")] {
        let path = html.split('"').find(|s| s.starts_with("./assets/") && s.ends_with(extension)).unwrap();
        let response = get(&app, &path[1..]).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["content-type"], mime);
        assert_eq!(response.headers()["cache-control"], "public, max-age=31536000, immutable");
        assert_eq!(response.headers()["x-content-type-options"], "nosniff");
        assert_eq!(
            to_bytes(response.into_body(), 1 << 20).await.unwrap().as_ref(),
            assets::lookup(&path[2..]).unwrap()
        );
    }
    for path in ["/assets/missing.js", "/assets/../index.html", "/assets/%2e%2e/index.html", "/assets/index.html"] {
        assert_eq!(get(&app, path).await.status(), StatusCode::NOT_FOUND);
    }
    assert!(assets::lookup("assets/../index.html").is_none());
    let response = get(&app, "/v1/stats").await;
    assert_eq!(response.headers()["cache-control"], "public, max-age=5");
    let rest: Value = serde_json::from_slice(&to_bytes(response.into_body(), 1 << 20).await.unwrap()).unwrap();
    assert_eq!(rest["version"], 1);
    assert_eq!(rest["seq"], 0);
    assert_eq!(rest["live"], serde_json::json!([]));
    // Last-Event-ID is deliberately ignored: every connection starts with a snapshot.
    let response = app
        .oneshot(
            Request::builder().uri("/v1/stats/stream").header("Last-Event-ID", "999999").body(Body::empty()).unwrap(),
        )
        .await
        .unwrap();
    let mut body = response.into_body().into_data_stream();
    assert_eq!(event(&mut body).await.0, "snapshot");
}

#[tokio::test]
async fn loopback_disconnect_releases_the_response_body_permit() {
    let app = router(AppState::new(
        config(),
        Catalog::parse(include_str!("../../../tests/fixtures/all-models.json")).unwrap(),
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = reqwest::Client::new();
    let url = format!("http://{address}/v1/stats/stream");
    let response = client.get(&url).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let mut stream = response.bytes_stream();
    assert!(stream.next().await.unwrap().unwrap().starts_with(b"event: snapshot"));
    assert_eq!(client.get(&url).send().await.unwrap().status(), StatusCode::SERVICE_UNAVAILABLE);
    drop(stream);
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if client.get(&url).send().await.unwrap().status() == StatusCode::OK {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("disconnected stream releases its permit");
    server.abort();
    let _ = server.await;
}
