//! Label verification, tokenization and the answer-slot forward pass, ported
//! from refactor-tool's `lightning.rs`, `lightning_transport.rs` and the
//! `single` policy of `ensemble/`. Constants and arithmetic are the original's.
use std::{collections::HashSet, fmt::Write, sync::Arc};

use pool_protocol::messages::JobErrorCode;
use serde_json::{Map, Value, json};

use super::prompt::{self, LABELS};
use crate::{LocalClient, RuntimeError};

const VOCAB_SIZE: u32 = 248_320;
/// Sampling temperature of the `single` policy.
const TEMPERATURE: f64 = 0.8;

fn failed(message: impl Into<String>) -> RuntimeError {
    RuntimeError::new(JobErrorCode::RuntimeFailed, message)
}

/// A started llama-server with verified labels. Cheap to clone.
#[derive(Clone)]
pub struct H2o {
    client: LocalClient,
    labels: Arc<[u32]>,
    /// Largest accepted prompt, in tokens (inclusive). The server's per-slot
    /// context is two tokens larger, as in refactor-tool's engine.
    token_limit: u32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Noul,
    Choice,
}

struct Prepared {
    id: String,
    kind: Kind,
    names: Vec<String>,
    tokens: Vec<u32>,
}

impl H2o {
    /// Checks that appending a space and each label A-P to the answer prompt adds
    /// exactly one distinct token, and keeps those token ids.
    pub async fn start(client: LocalClient, token_limit: u32) -> Result<Self, RuntimeError> {
        let mut engine = Self { client, labels: Arc::from(Vec::new()), token_limit };
        let probe = prompt::probe();
        let base = engine.tokenize(&probe).await?;
        let mut seen = HashSet::new();
        let mut labels = Vec::new();
        for label in LABELS {
            let full = engine.tokenize(&format!("{probe} {label}")).await?;
            if full.len() != base.len() + 1 || !full.starts_with(&base) || !seen.insert(full[base.len()]) {
                return Err(failed(format!("H2O label {label:?} is not one distinct space-prefixed token at Answer:")));
            }
            labels.push(full[base.len()]);
        }
        tracing::info!("verified H2O space-prefixed labels A-P: {labels:?}");
        engine.labels = labels.into();
        Ok(engine)
    }

    pub fn token_limit(&self) -> u32 {
        self.token_limit
    }

    async fn tokenize(&self, text: &str) -> Result<Vec<u32>, RuntimeError> {
        let reply = self
            .client
            .post_json("/tokenize", &json!({"content": text, "add_special": false, "parse_special": true}))
            .await?;
        let tokens: Vec<u32> = reply
            .get("tokens")
            .and_then(Value::as_array)
            .and_then(|tokens| tokens.iter().map(|id| id.as_u64().and_then(|id| u32::try_from(id).ok())).collect())
            .ok_or_else(|| failed("invalid /tokenize reply"))?;
        if tokens.iter().any(|&id| id >= VOCAB_SIZE) {
            return Err(failed("token ID exceeds pinned H2O vocabulary"));
        }
        Ok(tokens)
    }

    /// Renders and tokenizes every question of a systemone payload, in order.
    async fn prepare(&self, payload: &Value, enforce_limit: bool) -> Result<Vec<Prepared>, RuntimeError> {
        let invalid = |message: &str| RuntimeError::new(JobErrorCode::InvalidRequest, message.to_string());
        let state = payload.get("state").ok_or_else(|| invalid("\"state\" must be provided"))?;
        let questions = payload
            .get("questions")
            .and_then(Value::as_object)
            .ok_or_else(|| invalid("\"questions\" must be an object"))?;
        let mut prepared = Vec::with_capacity(questions.len());
        for (id, question) in questions {
            let (names, _) = prompt::options(question).map_err(|e| prefixed(id, e))?;
            let text = prompt::render(state, question).map_err(|e| prefixed(id, e))?;
            let tokens = self.tokenize(&text).await?;
            if enforce_limit && tokens.len() > self.token_limit as usize {
                return Err(RuntimeError::new(
                    JobErrorCode::ContextLengthExceeded,
                    format!(
                        "question {id:?}: the prompt is {} tokens; this profile allows {}",
                        tokens.len(),
                        self.token_limit
                    ),
                ));
            }
            let kind = if question["type"] == "noul" { Kind::Noul } else { Kind::Choice };
            prepared.push(Prepared { id: id.clone(), kind, names, tokens });
        }
        if prepared.is_empty() {
            return Err(invalid("\"questions\" must not be empty"));
        }
        Ok(prepared)
    }

    /// The longest prompt among the questions, in tokens. The limit is not
    /// enforced here, so the pool can answer `context_length_exceeded` itself.
    pub async fn count(&self, payload: &Value) -> Result<u32, RuntimeError> {
        let prepared = self.prepare(payload, false).await?;
        Ok(prepared.iter().map(|question| question.tokens.len() as u32).max().unwrap_or(0))
    }

    /// A systemone response: one forward per question, in order.
    /// `usage.input_tokens` is the sum of the tokens the server evaluated.
    pub async fn answer(&self, payload: &Value) -> Result<Value, RuntimeError> {
        let prepared = self.prepare(payload, true).await?;
        let mut answers = Map::new();
        let mut input_tokens = 0usize;
        for question in prepared {
            let (probabilities, evaluated) = self.forward(&question.tokens, question.names.len()).await?;
            input_tokens += evaluated;
            let answer = match question.kind {
                Kind::Noul => json!({"noul": commit_noul(probabilities[0])}),
                Kind::Choice => {
                    let map: Map<String, Value> =
                        question.names.into_iter().zip(probabilities).map(|(name, p)| (name, json!(p))).collect();
                    json!({"probabilities": map})
                }
            };
            answers.insert(question.id, answer);
        }
        Ok(json!({"answers": answers, "usage": {"input_tokens": input_tokens}}))
    }

    /// One label-only completion at the answer slot. Returns the probabilities
    /// of the first `options` labels and the prompt tokens the server evaluated.
    async fn forward(&self, tokens: &[u32], options: usize) -> Result<(Vec<f64>, usize), RuntimeError> {
        let ids = &self.labels[..options];
        let result = self.client.post_bytes("/completion", completion_body(tokens, ids)).await?;
        let evaluated = result["tokens_evaluated"].as_u64();
        if evaluated != Some(tokens.len() as u64) || result["truncated"].as_bool() != Some(false) {
            return Err(failed("H2O completion changed or truncated the verified prompt"));
        }
        let top = result["completion_probabilities"][0]["top_probs"]
            .as_array()
            .ok_or_else(|| failed("missing H2O answer-slot probabilities"))?;
        if top.len() != ids.len() {
            return Err(failed("H2O answer-slot readout omitted a label (possible float underflow)"));
        }
        let mut logprobs = Vec::with_capacity(ids.len());
        for id in ids {
            let entries: Vec<_> = top.iter().filter(|v| v["id"].as_u64() == Some(u64::from(*id))).collect();
            let p = if entries.len() == 1 { entries[0]["prob"].as_f64() } else { None };
            let p = p
                .filter(|p| p.is_finite() && *p > 0.0 && *p <= 1.0)
                .ok_or_else(|| failed(format!("missing or invalid H2O label token probability {id}")))?;
            logprobs.push(p.ln());
        }
        Ok((probabilities(&logprobs, TEMPERATURE), tokens.len()))
    }
}

fn prefixed(id: &str, error: RuntimeError) -> RuntimeError {
    RuntimeError::new(error.code, format!("question {id:?}: {}", error.message))
}

/// The label bias of `/completion`. Instead of masking all 248,320 other tokens
/// (a 3.8 MB body llama-server must parse and apply), every label gets this
/// positive bias. The labels' logits shift by the same constant, so their
/// softmax is unchanged and the other tokens' share shrinks by e^-64.
/// Margin vs rounding: a nonlabel must beat the weakest label by 64 nats to push a label out of `top_probs`
/// (measured gap is at most 1.3), while f32 rounding at 64 to 128 costs only about 5e-7, far below the server's own ~1e-3 noise.
const LABEL_BIAS: f64 = 64.0;

/// The body sent to `/completion`: the prompt tokens and the label bias, with
/// refactor-tool's sampling settings. Built as text; the prompt can be 40,000 tokens.
pub(super) fn completion_body(tokens: &[u32], labels: &[u32]) -> Vec<u8> {
    let mut body = String::with_capacity(tokens.len() * 7 + 512);
    body.push_str("{\"prompt\":[");
    for (i, id) in tokens.iter().enumerate() {
        if i > 0 {
            body.push(',');
        }
        let _ = write!(body, "{id}");
    }
    body.push_str("],\"logit_bias\":[");
    for (i, id) in labels.iter().enumerate() {
        if i > 0 {
            body.push(',');
        }
        let _ = write!(body, "[{id},{LABEL_BIAS:?}]");
    }
    let _ = write!(
        body,
        "],\"n_predict\":1,\"n_probs\":{},\"samplers\":[],\"post_sampling_probs\":true,\"cache_prompt\":true,\
         \"stream\":false,\"temperature\":1.0,\"backend_sampling\":false,\"mirostat\":0,\"seed\":0,\
         \"response_fields\":[\"completion_probabilities\",\"tokens_evaluated\",\"truncated\"]}}",
        labels.len()
    );
    body.into_bytes()
}

/// Stable softmax(logprobs / max(T, 1e-3)).
pub(super) fn probabilities(logprobs: &[f64], temperature: f64) -> Vec<f64> {
    let scaled = logprobs.iter().map(|x| x / temperature.max(1e-3)).collect::<Vec<_>>();
    let maximum = scaled.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let mut p = scaled.iter().map(|x| (x - maximum).exp()).collect::<Vec<_>>();
    let sum: f64 = p.iter().sum();
    for value in &mut p {
        *value /= sum;
    }
    p
}

/// Keep confident answers; ties commit to true.
pub(super) fn commit_noul(yes: f64) -> f64 {
    if yes.max(1.0 - yes) >= 0.801 {
        yes
    } else if yes >= 0.5 {
        0.801
    } else {
        1.0 - 0.801
    }
}
