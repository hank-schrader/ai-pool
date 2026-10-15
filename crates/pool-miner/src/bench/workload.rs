//! Seeded synthetic customer records and technical notes, compiled into the miner.

use std::future::Future;

use pool_protocol::{
    Backend, Model, Operation,
    requests::{self, Validated},
};
use serde_json::{Value, json};

pub const VERSION: u32 = 2;

const NOTES: [&str; 6] = [
    "The customer reported a duplicate charge after updating the delivery address. Support checked the order history and requested a receipt. ",
    "The parcel arrived at the regional warehouse on Tuesday. Tracking showed a delay, and the shipping team scheduled another delivery attempt. ",
    "The service recorded a timeout while reading the account settings. Engineers reviewed the logs and restarted the affected worker after the queue drained. ",
    "The invoice lists three items, a shipping fee and a discount. The refund request is waiting for approval from the billing team. ",
    "A maintenance report describes a replaced sensor and a successful inspection. The operator compared readings with the previous week's measurements. ",
    "The project uses a shared task queue. Workers reserve capacity before processing a record and release it when an answer is ready. ",
];

pub struct Generator(u64);

impl Generator {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e3779b97f4a7c15);
        let mut value = self.0;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d049bb133111eb);
        value ^ (value >> 31)
    }

    fn record_tokens(&mut self) -> usize {
        let (min, max) = match self.next() % 10 {
            0..=3 => (300, 1500),
            4..=6 => (1500, 4000),
            7..=8 => (4000, 12000),
            _ => (12000, 40000),
        };
        min + self.next() as usize % (max - min)
    }

    pub fn record(&mut self, model: &Model) -> Value {
        // Approximate corpus tokens at four bytes each; exact counting and
        // trimming happen later without changing the seeded record sequence.
        let bytes = self.record_tokens() * 4;
        let mut text = format!("Record {:016x}. ", self.next());
        while text.len() < bytes {
            let note = NOTES[self.next() as usize % NOTES.len()];
            text.push_str(note);
            text.push_str(&format!("Entry {}. ", self.next() % 10000));
        }
        match model.backend {
            Backend::LlamaServerChat => json!({
                "model": model.id,
                "messages": [{"role": "user", "content": format!("Summarize this record in a few sentences:\n{text}")}],
                "max_tokens": ([32, 64, 128][self.next() as usize % 3].min(model.limits.max_output_tokens.unwrap())),
                "seed": self.next() % i32::MAX as u64,
                "temperature": 0.2,
            }),
            _ => {
                let count = (1 + self.next() as usize % 8).min(model.limits.max_questions.unwrap());
                let mut questions = serde_json::Map::new();
                for index in 0..count {
                    let question = if self.next().is_multiple_of(2) {
                        json!({"type": "noul", "instructions": "Does the record describe an unresolved customer problem?"})
                    } else {
                        let options = (3 + self.next() as usize % 3).min(model.limits.max_choice_options.unwrap());
                        let criteria: serde_json::Map<_, _> =
                            ["billing", "shipping", "technical", "maintenance", "other"]
                                .into_iter()
                                .take(options)
                                .map(|name| (name.into(), Value::Null))
                                .collect();
                        json!({"type": "choice", "instructions": "Which team should review this record?", "criteria": criteria})
                    };
                    questions.insert(format!("q{index}"), question);
                }
                json!({"model": model.id, "state": text, "questions": questions})
            }
        }
    }
}

#[derive(Clone)]
pub struct Record {
    pub payload: Value,
    pub questions: u64,
}

pub async fn fit<F, Fut>(model: &Model, context: u32, mut body: Value, count: F) -> Result<Record, String>
where
    F: Fn(Value) -> Fut,
    Fut: Future<Output = Result<u32, String>>,
{
    let operation =
        if model.backend == Backend::LlamaServerChat { Operation::ChatCompletions } else { Operation::Systemone };
    for _ in 0..32 {
        let validated: Validated =
            requests::validate(operation, model, body.as_object().unwrap().clone()).map_err(|error| error.message)?;
        let tokens = count(validated.count_payload.clone()).await?;
        let required = validated.required_context(tokens);
        if required <= context.saturating_sub(32)
            && serde_json::to_vec(&body).unwrap().len() <= model.limits.request_bytes
        {
            let questions = validated.payload["questions"].as_object().map_or(0, |questions| questions.len() as u64);
            return Ok(Record { payload: validated.payload, questions });
        }
        let text =
            if operation == Operation::Systemone { &mut body["state"] } else { &mut body["messages"][0]["content"] };
        let value = text.as_str().unwrap();
        if value.len() < 64 {
            return Err("benchmark prompt template does not fit this profile".into());
        }
        // ASCII corpus: every byte boundary is a character boundary.
        let length = value.len() * 3 / 4;
        *text = value[..length].into();
    }
    Err("could not fit benchmark record within the context and request limits".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pool_protocol::Catalog;

    #[tokio::test]
    async fn reproducible_mix_fits_all_backends_and_limits() {
        let catalog = Catalog::parse(include_str!("../../../../tests/fixtures/all-models.json")).unwrap();
        for model in &catalog.models {
            let mut first = Generator::new(20261012);
            let mut second = Generator::new(20261012);
            let mut counts = std::collections::HashSet::new();
            for _ in 0..32 {
                let body = first.record(model);
                assert_eq!(body, second.record(model));
                let record =
                    fit(
                        model,
                        2048,
                        body,
                        |payload| async move { Ok(serde_json::to_vec(&payload).unwrap().len() as u32) },
                    )
                    .await
                    .unwrap();
                assert!(serde_json::to_vec(&record.payload).unwrap().len() < 2048);
                counts.insert(record.questions);
                if model.backend != Backend::LlamaServerChat {
                    assert!((1..=8).contains(&record.questions));
                }
            }
            if model.backend != Backend::LlamaServerChat {
                assert!(counts.len() > 1);
            }
        }
    }

    #[test]
    fn absolute_lengths_follow_the_pool_mix() {
        let mut generator = Generator::new(20261012);
        let mut buckets = [0usize; 4];
        for _ in 0..10000 {
            let bucket = match generator.record_tokens() {
                300..1500 => 0,
                1500..4000 => 1,
                4000..12000 => 2,
                12000..40000 => 3,
                _ => panic!("record length outside the workload distribution"),
            };
            buckets[bucket] += 1;
        }
        for (count, expected) in buckets.into_iter().zip([4000, 3000, 2000, 1000]) {
            assert!(count.abs_diff(expected) < 200);
        }
    }

    #[tokio::test]
    async fn profiles_share_records_and_only_trim_the_text() {
        let catalog = Catalog::parse(include_str!("../../../../tests/fixtures/all-models.json")).unwrap();
        for model in &catalog.models {
            let mut small = Generator::new(20261012);
            let mut large = Generator::new(20261012);
            for _ in 0..16 {
                let small_body = small.record(model);
                let large_body = large.record(model);
                assert_eq!(small_body, large_body);
                let count = |payload| async move { Ok(serde_json::to_vec(&payload).unwrap().len().div_ceil(4) as u32) };
                let small = fit(model, 2048, small_body, count).await.unwrap();
                let large = fit(model, 40960, large_body, count).await.unwrap();
                let text = |record: &Record| {
                    if model.backend == Backend::LlamaServerChat {
                        record.payload["messages"][0]["content"].as_str().unwrap().to_owned()
                    } else {
                        record.payload["state"].as_str().unwrap().to_owned()
                    }
                };
                assert!(text(&large).starts_with(&text(&small)));
                assert_eq!(small.questions, large.questions);
                assert_eq!(small.payload["questions"], large.payload["questions"]);
                assert_eq!(small.payload["max_tokens"], large.payload["max_tokens"]);
                assert_eq!(small.payload["seed"], large.payload["seed"]);
            }
        }
    }
}
