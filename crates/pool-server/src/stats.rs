//! Public totals, owned by the scheduler, with completed UTC second/minute buckets.

use serde::Serialize;
use std::{collections::VecDeque, sync::Arc, time::Duration};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::time::Instant;

const RATE_SECONDS: usize = 60;
const HISTORY_MINUTES: usize = 1440;
const LIVE_SECONDS: usize = 300;

#[derive(Clone, Debug, Serialize)]
pub struct StatsSnapshot {
    pub updated_at: String,
    pub miners: usize,
    pub queued_questions: u64,
    pub answering_questions: u64,
    pub average_prompt_tokens: Option<f64>,
    pub answered_per_second: f64,
    pub answered_last_24h: u64,
    pub history: Arc<History>,
    pub live: Arc<Vec<StatsPoint>>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct StatsPoint {
    pub t: String,
    pub answered: u64,
    pub queued: u64,
    pub answering: u64,
    pub miners: usize,
    pub requests: u64,
    pub prompt_tokens: u64,
}

#[derive(Debug, Serialize)]
pub struct History {
    pub interval_seconds: u64,
    pub end: String,
    pub answered: Vec<u64>,
    pub queued: Vec<u64>,
    pub answering: Vec<u64>,
    pub miners: Vec<usize>,
    pub requests: Vec<u64>,
    pub prompt_tokens: Vec<u64>,
}

#[derive(Clone, Copy, Default)]
struct Counts {
    answered: u64,
    requests: u64,
    prompt_tokens: u64,
}

struct Minute {
    counts: Counts,
    queued: u64,
    answering: u64,
    miners: usize,
}

pub(crate) struct Stats {
    started: Instant,
    unix_start: Duration,
    second: u64,
    seconds: [Counts; RATE_SECONDS],
    rolling: Counts,
    second_queue_peak: u64,
    minute: u64,
    minute_counts: Counts,
    minute_queue_peak: u64,
    minutes: VecDeque<Minute>,
    history_answered: u64,
    history: Arc<History>,
    live: Arc<Vec<StatsPoint>>,
}

impl Stats {
    pub(crate) fn new(started: Instant, unix_start: Duration) -> Self {
        let second = unix_start.as_secs();
        let minute = second / 60;
        Self {
            started,
            unix_start,
            second,
            seconds: [Counts::default(); RATE_SECONDS],
            rolling: Counts::default(),
            second_queue_peak: 0,
            minute,
            minute_counts: Counts::default(),
            minute_queue_peak: 0,
            minutes: VecDeque::with_capacity(HISTORY_MINUTES),
            history_answered: 0,
            live: Arc::new(Vec::new()),
            history: Arc::new(History {
                interval_seconds: 60,
                end: timestamp(minute.saturating_sub(1) * 60),
                answered: Vec::new(),
                queued: Vec::new(),
                answering: Vec::new(),
                miners: Vec::new(),
                requests: Vec::new(),
                prompt_tokens: Vec::new(),
            }),
        }
    }

    /// Observes questions left waiting after a dispatch pass, and at the next turn's start.
    pub(crate) fn observe_queue(&mut self, queued: u64) {
        self.second_queue_peak = self.second_queue_peak.max(queued);
        self.minute_queue_peak = self.minute_queue_peak.max(queued);
    }

    /// Use monotonic elapsed time so wall-clock adjustments cannot repeat buckets.
    pub(crate) fn advance(&mut self, now: Instant, queued: u64, miners: usize, answering: u64) {
        let second = (self.unix_start + now.saturating_duration_since(self.started)).as_secs().max(self.second);
        self.observe_queue(queued);
        if second > self.second {
            let first_full = self.unix_start.as_secs() + u64::from(self.unix_start.subsec_nanos() != 0);
            let from = self.second.max(first_full).max(second.saturating_sub(LIVE_SECONDS as u64));
            let mut live = self.live.as_ref().clone();
            for s in from..second {
                let counts =
                    if s == self.second { self.seconds[(s % RATE_SECONDS as u64) as usize] } else { Counts::default() };
                live.push(StatsPoint {
                    t: timestamp(s),
                    answered: counts.answered,
                    queued: if s == self.second { self.second_queue_peak } else { queued },
                    answering,
                    miners,
                    requests: counts.requests,
                    prompt_tokens: counts.prompt_tokens,
                });
            }
            if live.len() > LIVE_SECONDS {
                live.drain(..live.len() - LIVE_SECONDS);
            }
            self.live = Arc::new(live);
            self.second_queue_peak = queued;
        }
        if second - self.second >= RATE_SECONDS as u64 {
            self.seconds.fill(Counts::default());
            self.rolling = Counts::default();
        } else {
            for s in self.second + 1..=second {
                let bucket = &mut self.seconds[(s % RATE_SECONDS as u64) as usize];
                self.rolling.answered -= bucket.answered;
                self.rolling.requests -= bucket.requests;
                self.rolling.prompt_tokens -= bucket.prompt_tokens;
                *bucket = Counts::default();
            }
        }
        self.second = second;
        let minute = second / 60;
        if minute == self.minute {
            return;
        }
        if minute - self.minute > HISTORY_MINUTES as u64 {
            self.minutes.clear();
            self.history_answered = 0;
            self.minute_counts = Counts::default();
            self.minute_queue_peak = queued;
            self.minute = minute - HISTORY_MINUTES as u64;
        }
        while self.minute < minute {
            // The startup minute cannot provide a full-minute rate.
            if self.minute > self.unix_start.as_secs() / 60 {
                if self.minutes.len() == HISTORY_MINUTES {
                    self.history_answered -= self.minutes.pop_front().expect("history is full").counts.answered;
                }
                self.history_answered += self.minute_counts.answered;
                self.minutes.push_back(Minute {
                    counts: self.minute_counts,
                    queued: self.minute_queue_peak,
                    answering,
                    miners,
                });
            }
            self.minute_counts = Counts::default();
            self.minute_queue_peak = queued;
            self.minute += 1;
        }
        self.history = Arc::new(History {
            interval_seconds: 60,
            end: timestamp((minute - 1) * 60),
            answered: self.minutes.iter().map(|m| m.counts.answered).collect(),
            queued: self.minutes.iter().map(|m| m.queued).collect(),
            answering: self.minutes.iter().map(|m| m.answering).collect(),
            miners: self.minutes.iter().map(|m| m.miners).collect(),
            requests: self.minutes.iter().map(|m| m.counts.requests).collect(),
            prompt_tokens: self.minutes.iter().map(|m| m.counts.prompt_tokens).collect(),
        });
    }

    /// One delivered request, regardless of its number of questions or attempts.
    pub(crate) fn answered(&mut self, questions: u64, input_tokens: u32) {
        for counts in [
            &mut self.seconds[(self.second % RATE_SECONDS as u64) as usize],
            &mut self.rolling,
            &mut self.minute_counts,
        ] {
            counts.answered += questions;
            counts.requests += 1;
            counts.prompt_tokens += u64::from(input_tokens);
        }
    }

    pub(crate) fn snapshot(&self, queued_questions: u64, miners: usize, answering_questions: u64) -> StatsSnapshot {
        StatsSnapshot {
            updated_at: timestamp(self.second),
            miners,
            queued_questions,
            answering_questions,
            average_prompt_tokens: (self.rolling.requests > 0)
                .then(|| self.rolling.prompt_tokens as f64 / self.rolling.requests as f64),
            answered_per_second: self.rolling.answered as f64 / RATE_SECONDS as f64,
            answered_last_24h: self.history_answered,
            history: self.history.clone(),
            live: self.live.clone(),
        }
    }
}

fn timestamp(seconds: u64) -> String {
    OffsetDateTime::from_unix_timestamp(seconds as i64).expect("valid Unix time").format(&Rfc3339).expect("UTC time")
}

#[cfg(test)]
mod tests {
    use super::*;
    use pool_protocol::{Backend, Catalog, Operation, requests};
    use serde_json::json;

    #[test]
    fn waiting_peaks_survive_bursts_and_reset_at_second_and_minute_boundaries() {
        let now = Instant::now();
        let mut stats = Stats::new(now, Duration::from_secs(120));
        stats.observe_queue(99); // Startup minute is excluded.
        stats.advance(now + Duration::from_secs(60), 0, 1, 4);
        stats.observe_queue(7);
        stats.observe_queue(3);
        stats.advance(now + Duration::from_secs(61), 0, 1, 4);
        assert_eq!(stats.snapshot(0, 1, 4).live.last().unwrap().queued, 7);
        assert_eq!(stats.snapshot(0, 1, 4).live.last().unwrap().answering, 4);
        stats.advance(now + Duration::from_secs(62), 0, 1, 2);
        assert_eq!(stats.snapshot(0, 1, 2).live.last().unwrap().queued, 0);
        stats.observe_queue(5);
        stats.advance(now + Duration::from_secs(120), 0, 2, 1);
        assert_eq!(stats.snapshot(0, 2, 1).history.queued, [7]);
        assert_eq!(stats.snapshot(0, 2, 1).history.answering, [1]);
        stats.advance(now + Duration::from_secs(121), 0, 2, 0);
        assert_eq!(stats.snapshot(0, 2, 0).live.last().unwrap().queued, 0);
        stats.observe_queue(2);
        stats.advance(now + Duration::from_secs(180), 0, 2, 0);
        assert_eq!(stats.snapshot(0, 2, 0).history.queued, [7, 2]);
        assert_eq!(stats.snapshot(0, 2, 0).queued_questions, 0);
    }

    #[test]
    fn prompt_average_counts_requests_once_and_expires_with_the_rolling_window() {
        let now = Instant::now();
        let mut stats = Stats::new(now, Duration::from_secs(120));
        assert_eq!(stats.snapshot(0, 0, 0).average_prompt_tokens, None);
        stats.answered(4, 1000);
        stats.answered(1, 2000);
        assert_eq!(stats.snapshot(0, 0, 0).average_prompt_tokens, Some(1500.0));
        stats.advance(now + Duration::from_secs(1), 0, 0, 0);
        let snapshot = stats.snapshot(0, 0, 0);
        assert_eq!(snapshot.live[0].answered, 5);
        assert_eq!(snapshot.live[0].requests, 2);
        assert_eq!(snapshot.live[0].prompt_tokens, 3000);
        stats.advance(now + Duration::from_secs(59), 0, 0, 0);
        stats.answered(3, 9000);
        assert_eq!(stats.snapshot(0, 0, 0).average_prompt_tokens, Some(4000.0));
        stats.advance(now + Duration::from_secs(60), 0, 0, 0);
        assert_eq!(stats.snapshot(0, 0, 0).average_prompt_tokens, Some(9000.0));
        stats.advance(now + Duration::from_secs(119), 0, 0, 0);
        assert_eq!(stats.snapshot(0, 0, 0).average_prompt_tokens, None);
        stats.answered(1, 0);
        assert_eq!(stats.snapshot(0, 0, 0).average_prompt_tokens, Some(0.0));
        stats.advance(now + Duration::from_secs(120), 0, 0, 0);
        let snapshot = stats.snapshot(0, 0, 0);
        assert_eq!(snapshot.history.requests, [1]);
        assert_eq!(snapshot.history.prompt_tokens, [0]);
        assert_eq!(snapshot.live.iter().filter(|p| p.t >= timestamp(180)).map(|p| p.requests).sum::<u64>(), 1);
        stats.advance(now + Duration::from_secs(1_000_000), 0, 0, 0);
        let snapshot = stats.snapshot(0, 0, 0);
        assert_eq!(snapshot.average_prompt_tokens, None);
        assert!(snapshot.history.requests.iter().all(|n| *n == 0));
        assert!(snapshot.history.prompt_tokens.iter().all(|n| *n == 0));
    }

    #[test]
    fn seconds_skip_partial_startup_and_only_copy_at_boundaries() {
        let now = Instant::now();
        let mut stats = Stats::new(now, Duration::from_millis(150_500));
        stats.answered(3, 100);
        let initial = stats.snapshot(0, 0, 0);
        stats.advance(now + Duration::from_millis(499), 7, 2, 0);
        assert!(Arc::ptr_eq(&initial.live, &stats.snapshot(7, 2, 0).live));
        stats.advance(now + Duration::from_millis(500), 7, 2, 0);
        assert!(stats.snapshot(7, 2, 0).live.is_empty());
        stats.answered(4, 100);
        stats.advance(now + Duration::from_millis(1500), 8, 3, 0);
        let snapshot = stats.snapshot(8, 3, 0);
        assert_eq!(
            snapshot.live.as_ref(),
            &[StatsPoint {
                t: timestamp(151),
                answered: 4,
                queued: 8,
                answering: 0,
                miners: 3,
                requests: 1,
                prompt_tokens: 100
            }]
        );
        assert_eq!(snapshot.answered_per_second, 7.0 / 60.0);
        assert_eq!(snapshot.answered_last_24h, 0);
        stats.advance(now + Duration::from_millis(1599), 9, 4, 0);
        assert!(Arc::ptr_eq(&snapshot.live, &stats.snapshot(9, 4, 0).live));
    }

    #[test]
    fn live_is_bounded_and_catch_up_samples_last_known_boundary_state() {
        let now = Instant::now();
        let mut stats = Stats::new(now, Duration::from_secs(120));
        stats.answered(9, 100);
        stats.advance(now + Duration::from_secs(3), 4, 2, 0);
        assert_eq!(stats.snapshot(4, 2, 0).live.iter().map(|p| p.answered).collect::<Vec<_>>(), [9, 0, 0]);
        stats.answered(7, 100);
        stats.advance(now + Duration::from_secs(1_000_000), 8, 3, 0);
        let snapshot = stats.snapshot(8, 3, 0);
        assert_eq!(snapshot.live.len(), LIVE_SECONDS);
        assert_eq!(snapshot.live.first().unwrap().t, timestamp(120 + 1_000_000 - 300));
        assert_eq!(snapshot.live.last().unwrap().t, timestamp(120 + 1_000_000 - 1));
        assert!(snapshot.live.iter().all(|p| p.answered == 0 && p.queued == 8 && p.miners == 3));
    }

    #[test]
    fn completed_full_minute_is_the_sum_of_its_completed_seconds() {
        let now = Instant::now();
        let mut stats = Stats::new(now, Duration::from_millis(150_500));
        stats.answered(99, 100);
        stats.advance(now + Duration::from_millis(29_500), 0, 0, 0);
        for second in 0..60 {
            stats.answered(second + 1, 100);
            stats.advance(now + Duration::from_millis(29_500) + Duration::from_secs(second + 1), 2, 1, 0);
        }
        let snapshot = stats.snapshot(2, 1, 0);
        let sum: u64 = snapshot.live.iter().filter(|p| p.t >= timestamp(180)).map(|p| p.answered).sum();
        assert_eq!(sum, (1..=60).sum::<u64>());
        assert_eq!(snapshot.history.answered, [sum]);
        assert_eq!(snapshot.history.requests, [60]);
        assert_eq!(snapshot.history.prompt_tokens, [6000]);
        assert_eq!(snapshot.live.iter().filter(|p| p.t >= timestamp(180)).map(|p| p.requests).sum::<u64>(), 60);
        assert_eq!(snapshot.live.iter().filter(|p| p.t >= timestamp(180)).map(|p| p.prompt_tokens).sum::<u64>(), 6000);
        assert_eq!(snapshot.answered_last_24h, sum);
        assert_eq!(snapshot.live.last().unwrap().t, timestamp(239));
    }

    #[test]
    fn validated_requests_count_systemone_questions_and_one_chat_completion() {
        let catalog = Catalog::parse(include_str!("../../../tests/fixtures/all-models.json")).unwrap();
        for model in &catalog.models {
            let (operation, body, expected) = if model.backend == Backend::LlamaServerChat {
                (Operation::ChatCompletions, json!({"messages": [{"role": "user", "content": "hi"}]}), 1)
            } else {
                (
                    Operation::Systemone,
                    json!({"state": "hi", "questions": {
                        "a": {"type": "noul", "instructions": "a"},
                        "b": {"type": "noul", "instructions": "b"},
                        "c": {"type": "noul", "instructions": "c"}
                    }}),
                    3,
                )
            };
            let request = requests::validate(operation, model, body.as_object().unwrap().clone()).unwrap();
            assert_eq!(request.question_count, expected);
        }
    }

    #[test]
    fn rolling_rate_expires_second_buckets_and_handles_gaps() {
        let now = Instant::now();
        let mut stats = Stats::new(now, Duration::from_secs(120));
        stats.answered(120, 100);
        assert_eq!(stats.snapshot(0, 0, 0).answered_per_second, 2.0);
        stats.advance(now + Duration::from_secs(59), 0, 0, 0);
        stats.answered(60, 100);
        assert_eq!(stats.snapshot(0, 0, 0).answered_per_second, 3.0);
        stats.advance(now + Duration::from_secs(60), 0, 0, 0);
        assert_eq!(stats.snapshot(0, 0, 0).answered_per_second, 1.0);
        stats.advance(now + Duration::from_secs(119), 0, 0, 0);
        assert_eq!(stats.snapshot(0, 0, 0).answered_per_second, 0.0);
        stats.answered(180, 100);
        stats.advance(now + Duration::from_secs(300), 0, 0, 0);
        assert_eq!(stats.snapshot(0, 0, 0).answered_per_second, 0.0);
    }

    #[test]
    fn history_skips_startup_minute_and_samples_full_minutes_at_the_boundary() {
        let now = Instant::now();
        let mut stats = Stats::new(now, Duration::from_millis(150_500));
        stats.answered(3, 100);
        stats.advance(now + Duration::from_millis(29_499), 7, 2, 0);
        let snapshot = stats.snapshot(7, 2, 0);
        assert!(snapshot.history.answered.is_empty());
        assert_eq!(snapshot.answered_last_24h, 0);
        stats.advance(now + Duration::from_millis(29_500), 5, 1, 0);
        stats.answered(4, 100);
        let snapshot = stats.snapshot(8, 3, 0);
        assert_eq!(snapshot.history.end, "1970-01-01T00:02:00Z");
        assert!(snapshot.history.answered.is_empty());
        assert!(snapshot.history.queued.is_empty());
        assert!(snapshot.history.miners.is_empty());
        assert_eq!(snapshot.answered_last_24h, 0);
        assert_eq!(snapshot.answered_per_second, 7.0 / 60.0);
        assert_eq!(snapshot.queued_questions, 8);
        assert_eq!(snapshot.miners, 3);
        stats.advance(now + Duration::from_millis(89_500), 9, 4, 0);
        let snapshot = stats.snapshot(9, 4, 0);
        assert_eq!(snapshot.history.end, "1970-01-01T00:03:00Z");
        assert_eq!(snapshot.history.answered, [4]);
        assert_eq!(snapshot.history.queued, [9]);
        assert_eq!(snapshot.history.miners, [4]);
        assert_eq!(snapshot.answered_last_24h, 4);
        stats.answered(6, 100);
        stats.advance(now + Duration::from_millis(149_500), 2, 1, 0);
        let snapshot = stats.snapshot(2, 1, 0);
        assert_eq!(snapshot.history.answered, [4, 6]);
        assert_eq!(snapshot.history.queued, [9, 9]);
        assert_eq!(snapshot.history.miners, [4, 1]);
        assert_eq!(snapshot.answered_last_24h, 10);
    }

    #[test]
    fn history_is_oldest_first_and_capped_at_1440_minutes() {
        let now = Instant::now();
        let mut stats = Stats::new(now, Duration::from_secs(120));
        stats.advance(now + Duration::from_secs(60), 0, 0, 0);
        for minute in 0..1441 {
            stats.answered(minute + 1, 100);
            stats.advance(now + Duration::from_secs((minute + 2) * 60), minute, minute as usize, 0);
        }
        let snapshot = stats.snapshot(0, 0, 0);
        assert_eq!(snapshot.history.answered.len(), HISTORY_MINUTES);
        assert_eq!(snapshot.history.answered.first(), Some(&2));
        assert_eq!(snapshot.history.answered.last(), Some(&1441));
        assert_eq!(snapshot.history.queued.first(), Some(&1));
        assert_eq!(snapshot.history.miners.last(), Some(&1440));
        assert_eq!(snapshot.answered_last_24h, (2..=1441).sum::<u64>());
        stats.advance(now + Duration::from_secs(60 * 4000), 6, 2, 0);
        let snapshot = stats.snapshot(6, 2, 0);
        assert_eq!(snapshot.history.answered, vec![0; HISTORY_MINUTES]);
        assert_eq!(snapshot.history.queued, vec![6; HISTORY_MINUTES]);
        assert_eq!(snapshot.history.miners, vec![2; HISTORY_MINUTES]);
        assert_eq!(snapshot.answered_last_24h, 0);
    }
}
