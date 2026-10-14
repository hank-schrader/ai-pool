//! Public totals, owned by the scheduler. History contains completed UTC minutes.

use std::{collections::VecDeque, sync::Arc, time::Duration};

use serde::Serialize;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::time::Instant;

const RATE_SECONDS: usize = 60;
const HISTORY_MINUTES: usize = 1440;

#[derive(Clone, Debug, Serialize)]
pub struct StatsSnapshot {
    pub updated_at: String,
    pub miners: usize,
    pub queued_questions: u64,
    pub answered_per_second: f64,
    pub answered_last_24h: u64,
    pub history: Arc<History>,
}

#[derive(Debug, Serialize)]
pub struct History {
    pub interval_seconds: u64,
    pub end: String,
    pub answered: Vec<u64>,
    pub queued: Vec<u64>,
    pub miners: Vec<usize>,
}

struct Minute {
    answered: u64,
    queued: u64,
    miners: usize,
}

pub(crate) struct Stats {
    started: Instant,
    unix_start: Duration,
    second: u64,
    seconds: [u64; RATE_SECONDS],
    rolling_answered: u64,
    minute: u64,
    minute_answered: u64,
    minutes: VecDeque<Minute>,
    history_answered: u64,
    history: Arc<History>,
}

impl Stats {
    pub(crate) fn new(started: Instant, unix_start: Duration) -> Self {
        let second = unix_start.as_secs();
        let minute = second / 60;
        Self {
            started,
            unix_start,
            second,
            seconds: [0; RATE_SECONDS],
            rolling_answered: 0,
            minute,
            minute_answered: 0,
            minutes: VecDeque::with_capacity(HISTORY_MINUTES),
            history_answered: 0,
            history: Arc::new(History {
                interval_seconds: 60,
                end: timestamp(minute.saturating_sub(1) * 60),
                answered: Vec::new(),
                queued: Vec::new(),
                miners: Vec::new(),
            }),
        }
    }

    /// Use monotonic elapsed time so wall-clock adjustments cannot repeat buckets.
    pub(crate) fn advance(&mut self, now: Instant, queued: u64, miners: usize) {
        let second = (self.unix_start + now.saturating_duration_since(self.started)).as_secs().max(self.second);
        if second - self.second >= RATE_SECONDS as u64 {
            self.seconds.fill(0);
            self.rolling_answered = 0;
        } else {
            for s in self.second + 1..=second {
                let bucket = &mut self.seconds[(s % RATE_SECONDS as u64) as usize];
                self.rolling_answered -= *bucket;
                *bucket = 0;
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
            self.minute_answered = 0;
            self.minute = minute - HISTORY_MINUTES as u64;
        }
        while self.minute < minute {
            // The startup minute cannot provide a full-minute rate.
            if self.minute > self.unix_start.as_secs() / 60 {
                if self.minutes.len() == HISTORY_MINUTES {
                    self.history_answered -= self.minutes.pop_front().expect("history is full").answered;
                }
                self.history_answered += self.minute_answered;
                self.minutes.push_back(Minute { answered: self.minute_answered, queued, miners });
            }
            self.minute_answered = 0;
            self.minute += 1;
        }
        self.history = Arc::new(History {
            interval_seconds: 60,
            end: timestamp((minute - 1) * 60),
            answered: self.minutes.iter().map(|m| m.answered).collect(),
            queued: self.minutes.iter().map(|m| m.queued).collect(),
            miners: self.minutes.iter().map(|m| m.miners).collect(),
        });
    }

    pub(crate) fn answered(&mut self, questions: u64) {
        self.seconds[(self.second % RATE_SECONDS as u64) as usize] += questions;
        self.rolling_answered += questions;
        self.minute_answered += questions;
    }

    pub(crate) fn snapshot(&self, queued_questions: u64, miners: usize) -> StatsSnapshot {
        StatsSnapshot {
            updated_at: timestamp(self.second),
            miners,
            queued_questions,
            answered_per_second: self.rolling_answered as f64 / RATE_SECONDS as f64,
            answered_last_24h: self.history_answered,
            history: self.history.clone(),
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
    fn validated_requests_count_systemone_questions_and_one_chat_completion() {
        let catalog = Catalog::parse(include_str!("../../../config/models.json")).unwrap();
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
        stats.answered(120);
        assert_eq!(stats.snapshot(0, 0).answered_per_second, 2.0);
        stats.advance(now + Duration::from_secs(59), 0, 0);
        stats.answered(60);
        assert_eq!(stats.snapshot(0, 0).answered_per_second, 3.0);
        stats.advance(now + Duration::from_secs(60), 0, 0);
        assert_eq!(stats.snapshot(0, 0).answered_per_second, 1.0);
        stats.advance(now + Duration::from_secs(119), 0, 0);
        assert_eq!(stats.snapshot(0, 0).answered_per_second, 0.0);
        stats.answered(180);
        stats.advance(now + Duration::from_secs(300), 0, 0);
        assert_eq!(stats.snapshot(0, 0).answered_per_second, 0.0);
    }

    #[test]
    fn history_skips_startup_minute_and_samples_full_minutes_at_the_boundary() {
        let now = Instant::now();
        let mut stats = Stats::new(now, Duration::from_millis(150_500));
        stats.answered(3);
        stats.advance(now + Duration::from_millis(29_499), 7, 2);
        let snapshot = stats.snapshot(7, 2);
        assert!(snapshot.history.answered.is_empty());
        assert_eq!(snapshot.answered_last_24h, 0);
        stats.advance(now + Duration::from_millis(29_500), 5, 1);
        stats.answered(4);
        let snapshot = stats.snapshot(8, 3);
        assert_eq!(snapshot.history.end, "1970-01-01T00:02:00Z");
        assert!(snapshot.history.answered.is_empty());
        assert!(snapshot.history.queued.is_empty());
        assert!(snapshot.history.miners.is_empty());
        assert_eq!(snapshot.answered_last_24h, 0);
        assert_eq!(snapshot.answered_per_second, 7.0 / 60.0);
        assert_eq!(snapshot.queued_questions, 8);
        assert_eq!(snapshot.miners, 3);
        stats.advance(now + Duration::from_millis(89_500), 9, 4);
        let snapshot = stats.snapshot(9, 4);
        assert_eq!(snapshot.history.end, "1970-01-01T00:03:00Z");
        assert_eq!(snapshot.history.answered, [4]);
        assert_eq!(snapshot.history.queued, [9]);
        assert_eq!(snapshot.history.miners, [4]);
        assert_eq!(snapshot.answered_last_24h, 4);
        stats.answered(6);
        stats.advance(now + Duration::from_millis(149_500), 2, 1);
        let snapshot = stats.snapshot(2, 1);
        assert_eq!(snapshot.history.answered, [4, 6]);
        assert_eq!(snapshot.history.queued, [9, 2]);
        assert_eq!(snapshot.history.miners, [4, 1]);
        assert_eq!(snapshot.answered_last_24h, 10);
    }

    #[test]
    fn history_is_oldest_first_and_capped_at_1440_minutes() {
        let now = Instant::now();
        let mut stats = Stats::new(now, Duration::from_secs(120));
        stats.advance(now + Duration::from_secs(60), 0, 0);
        for minute in 0..1441 {
            stats.answered(minute + 1);
            stats.advance(now + Duration::from_secs((minute + 2) * 60), minute, minute as usize);
        }
        let snapshot = stats.snapshot(0, 0);
        assert_eq!(snapshot.history.answered.len(), HISTORY_MINUTES);
        assert_eq!(snapshot.history.answered.first(), Some(&2));
        assert_eq!(snapshot.history.answered.last(), Some(&1441));
        assert_eq!(snapshot.history.queued.first(), Some(&1));
        assert_eq!(snapshot.history.miners.last(), Some(&1440));
        assert_eq!(snapshot.answered_last_24h, (2..=1441).sum::<u64>());
        stats.advance(now + Duration::from_secs(60 * 4000), 6, 2);
        let snapshot = stats.snapshot(6, 2);
        assert_eq!(snapshot.history.answered, vec![0; HISTORY_MINUTES]);
        assert_eq!(snapshot.history.queued, vec![6; HISTORY_MINUTES]);
        assert_eq!(snapshot.history.miners, vec![2; HISTORY_MINUTES]);
        assert_eq!(snapshot.answered_last_24h, 0);
    }
}
