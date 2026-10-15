//! One bounded, shared publication for all public statistics readers.

use std::{
    convert::Infallible,
    sync::{Arc, LazyLock},
    time::Duration,
};

use axum::{
    http::{StatusCode, header},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
};
use futures_util::stream;
use serde::Serialize;
use tokio::{
    sync::{Semaphore, watch},
    time::MissedTickBehavior,
};
use uuid::Uuid;

use crate::stats::{History, StatsPoint, StatsSnapshot};

static EPOCH: LazyLock<String> = LazyLock::new(|| Uuid::new_v4().to_string());

#[derive(Clone, Debug, Serialize)]
pub struct Totals {
    pub miners: usize,
    pub queued_questions: u64,
    pub answering_questions: u64,
    pub average_prompt_tokens: Option<f64>,
    pub answered_last_24h: u64,
    pub answered_last_hour: u64,
    pub answered_per_second: f64,
}

#[derive(Clone, Debug, Serialize)]
pub struct SharedFields {
    pub version: u8,
    pub epoch: String,
    pub seq: u64,
    pub updated_at: String,
    pub totals: Totals,
}

#[derive(Clone, Debug, Serialize)]
pub struct SnapshotPayload {
    #[serde(flatten)]
    pub shared: SharedFields,
    pub live: Arc<Vec<StatsPoint>>,
    pub history: Arc<History>,
}

#[derive(Debug, Serialize)]
pub struct TickPayload {
    #[serde(flatten)]
    pub shared: SharedFields,
    pub second: StatsPoint,
    pub minute: Option<StatsPoint>,
}

#[derive(Serialize)]
struct RestPayload<'a> {
    version: u8,
    epoch: &'a str,
    seq: u64,
    #[serde(flatten)]
    stats: &'a StatsSnapshot,
}

/// Each tick keeps the full snapshot of exactly the same publication.
#[derive(Debug)]
pub struct PublishedStats {
    pub snapshot: SnapshotPayload,
    pub snapshot_json: Arc<str>,
    pub tick_json: Option<Arc<str>>,
    pub rest_json: Arc<str>,
}

impl PublishedStats {
    fn new(stats: StatsSnapshot, seq: u64, previous: Option<&Self>) -> Self {
        let shared = SharedFields {
            version: 1,
            epoch: EPOCH.clone(),
            seq,
            updated_at: stats.updated_at.clone(),
            totals: Totals {
                miners: stats.miners,
                queued_questions: stats.queued_questions,
                answering_questions: stats.answering_questions,
                average_prompt_tokens: stats.average_prompt_tokens,
                answered_last_24h: stats.answered_last_24h,
                answered_last_hour: stats.answered_last_hour,
                answered_per_second: stats.answered_per_second,
            },
        };
        let tick_json = previous.and_then(|previous| {
            // A first point or adjacent retained endpoints prove no seconds were skipped.
            let adjacent = match previous.snapshot.live.last() {
                Some(prior) => stats.live.len() >= 2 && stats.live[stats.live.len() - 2].t == prior.t,
                None => stats.live.len() == 1,
            };
            if !adjacent {
                return None;
            }
            let history = &stats.history;
            let minute = (history.end != previous.snapshot.history.end && !history.answered.is_empty()).then(|| {
                let i = history.answered.len() - 1;
                StatsPoint {
                    t: history.end.clone(),
                    answered: history.answered[i],
                    queued: history.queued[i],
                    miners: history.miners[i],
                    answering: history.answering[i],
                    requests: history.requests[i],
                    prompt_tokens: history.prompt_tokens[i],
                }
            });
            Some(
                serde_json::to_string(&TickPayload {
                    shared: shared.clone(),
                    second: stats.live.last().expect("nonempty live history").clone(),
                    minute,
                })
                .expect("serializable statistics")
                .into(),
            )
        });
        let rest_json = serde_json::to_string(&RestPayload { version: 1, epoch: &shared.epoch, seq, stats: &stats })
            .expect("serializable statistics")
            .into();
        let snapshot = SnapshotPayload { shared, live: stats.live, history: stats.history };
        let snapshot_json = serde_json::to_string(&snapshot).expect("serializable statistics").into();
        Self { snapshot, snapshot_json, tick_json, rest_json }
    }
}

#[derive(Clone)]
pub struct StatsPublisher {
    publications: watch::Receiver<Arc<PublishedStats>>,
    streams: Arc<Semaphore>,
}

impl StatsPublisher {
    pub fn spawn(scheduler: watch::Receiver<StatsSnapshot>, capacity: usize) -> Self {
        let initial = Arc::new(PublishedStats::new(scheduler.borrow().clone(), 0, None));
        let (sender, publications) = watch::channel(initial);
        tokio::spawn(async move {
            let mut closed = scheduler.clone();
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        let stats = scheduler.borrow().clone();
                        let previous = sender.borrow().clone();
                        if stats.live.last().map(|p| &p.t) == previous.snapshot.live.last().map(|p| &p.t) {
                            continue;
                        }
                        let seq = previous.snapshot.shared.seq.checked_add(1).expect("statistics sequence exhausted");
                        sender.send_replace(Arc::new(PublishedStats::new(stats, seq, Some(&previous))));
                    }
                    // Wait only for closure; scheduler updates never trigger serialization.
                    _ = async { let _ = closed.wait_for(|_| false).await; } => return,
                }
            }
        });
        Self { publications, streams: Arc::new(Semaphore::new(capacity)) }
    }

    pub fn latest(&self) -> Arc<PublishedStats> {
        self.publications.borrow().clone()
    }

    pub fn response(&self) -> Response {
        let Ok(permit) = self.streams.clone().try_acquire_owned() else {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                [(header::RETRY_AFTER, "30"), (header::CACHE_CONTROL, "no-store")],
                "statistics stream capacity reached",
            )
                .into_response();
        };
        let events =
            stream::unfold((self.publications.clone(), permit, None), |(mut receiver, permit, last)| async move {
                if last.is_some() && receiver.changed().await.is_err() {
                    return None;
                }
                let frame = receiver.borrow_and_update().clone();
                let seq = frame.snapshot.shared.seq;
                let (name, data) = if last.is_some_and(|last: u64| last.checked_add(1) == Some(seq)) {
                    match &frame.tick_json {
                        Some(tick) => ("tick", tick),
                        None => ("snapshot", &frame.snapshot_json),
                    }
                } else {
                    ("snapshot", &frame.snapshot_json)
                };
                let event = Event::default().event(name).data(data.as_ref());
                Some((Ok::<_, Infallible>(event), (receiver, permit, Some(seq))))
            });
        let mut response =
            Sse::new(events).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))).into_response();
        response.headers_mut().insert(header::CACHE_CONTROL, "no-store, no-transform".parse().unwrap());
        response
    }
}
