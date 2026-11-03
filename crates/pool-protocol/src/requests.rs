//! Client request validation. The pool forwards only fields it understands;
//! anything it would silently ignore is rejected instead.

mod chat;
mod options;
mod routing;
mod systemone;
pub use routing::Routing;

use serde_json::{Map, Value, json};

use crate::{
    catalog::{Backend, Model},
    messages::Operation,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestError {
    pub code: &'static str,
    pub message: String,
    pub param: Option<String>,
}

impl RequestError {
    fn invalid(message: impl Into<String>, param: impl Into<Option<String>>) -> Self {
        Self { code: "invalid_request", message: message.into(), param: param.into() }
    }

    fn unsupported(param: &str, message: impl Into<String>) -> Self {
        Self { code: "unsupported_parameter", message: message.into(), param: Some(param.into()) }
    }
}

/// A request ready for scheduling.
#[derive(Debug, Clone)]
pub struct Validated {
    pub operation: Operation,
    /// Questions counted once when the job completes successfully.
    pub question_count: u64,
    pub stream: bool,
    /// Client asked for the final usage chunk of a stream.
    pub include_usage: bool,
    /// Tokens reserved for output on top of the counted input.
    pub output_tokens: u32,
    /// Pool-only minimum loaded profile context; never forwarded to a miner.
    pub min_context_tokens: Option<u32>,
    pub routing: Routing,
    /// Body the miner sends to its runtime.
    pub payload: Value,
    /// Body the miner counts input tokens of.
    pub count_payload: Value,
}

impl Validated {
    pub fn required_context(&self, input_tokens: u32) -> u32 {
        match self.operation {
            Operation::Systemone => input_tokens,
            // one extra token keeps generation clear of the context boundary
            Operation::ChatCompletions => input_tokens.saturating_add(self.output_tokens).saturating_add(1),
        }
    }
}

/// `body` has already been parsed and its `model` resolved to `model`.
pub fn validate(operation: Operation, model: &Model, body: Map<String, Value>) -> Result<Validated, RequestError> {
    match (operation, model.backend) {
        (Operation::Systemone, Backend::LlamaServerSystemone | Backend::LlamaServerH2o) => {
            systemone::validate(model, body)
        }
        (Operation::ChatCompletions, Backend::LlamaServerChat) => chat::validate(model, body),
        _ => Err(RequestError {
            code: "unsupported_capability",
            message: format!(
                "model {:?} serves {}; use its endpoint instead",
                model.id,
                match model.backend {
                    Backend::LlamaServerSystemone | Backend::LlamaServerH2o => "POST /v1/systemone",
                    Backend::LlamaServerChat => "POST /v1/chat/completions",
                }
            ),
            param: Some("model".into()),
        }),
    }
}

fn min_context_tokens(model: &Model, body: &mut Map<String, Value>) -> Result<Option<u32>, RequestError> {
    let Some(value) = body.remove("min_context_tokens") else { return Ok(None) };
    let max = model.max_context_tokens();
    match value.as_u64() {
        Some(tokens) if tokens > 0 && tokens <= u64::from(max) => Ok(Some(tokens as u32)),
        _ => Err(RequestError::invalid(
            format!("min_context_tokens must be an integer from 1 to {max}"),
            Some("min_context_tokens".into()),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::Catalog;

    fn catalog() -> Catalog {
        Catalog::parse(include_str!("../../../tests/fixtures/all-models.json")).unwrap()
    }

    fn body(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    #[test]
    fn minimum_context_is_validated_and_stripped_on_both_endpoints() {
        let catalog = catalog();
        for model in &catalog.models {
            let operation = if model.backend == Backend::LlamaServerChat {
                Operation::ChatCompletions
            } else {
                Operation::Systemone
            };
            let base = if operation == Operation::ChatCompletions {
                json!({"messages": [{"role": "user", "content": "hi"}], "max_tokens": 64})
            } else {
                json!({"state": "hi", "questions": {"q": {"type": "noul", "instructions": "x"}}})
            };
            let original = validate(operation, model, body(base.clone())).unwrap();
            assert_eq!(original.min_context_tokens, None);
            for tokens in [1, 8192, model.max_context_tokens()] {
                let mut request = base.clone();
                request["min_context_tokens"] = tokens.into();
                let validated = validate(operation, model, body(request)).unwrap();
                assert_eq!(validated.min_context_tokens, Some(tokens));
                assert_eq!(validated.payload, original.payload);
                assert_eq!(validated.count_payload, original.count_payload);
                assert_eq!(validated.required_context(100), original.required_context(100));
            }
            for invalid in [
                json!(0),
                json!(-1),
                json!(1.0),
                json!(1.5),
                json!(true),
                json!("8192"),
                Value::Null,
                json!([]),
                json!({}),
                json!(u64::MAX),
                json!(model.max_context_tokens() + 1),
            ] {
                let mut request = base.clone();
                request["min_context_tokens"] = invalid;
                let error = validate(operation, model, body(request)).unwrap_err();
                assert_eq!(error.code, "invalid_request");
                assert_eq!(error.param.as_deref(), Some("min_context_tokens"));
            }
        }
    }

    #[test]
    fn systemone_strips_model_and_keeps_questions() {
        let catalog = catalog();
        let clef = catalog.model("clef").unwrap();
        let validated = validate(
            Operation::Systemone,
            clef,
            body(json!({"model": "clef", "state": "hi", "questions": {"q": {"type": "noul", "instructions": "x"}}})),
        )
        .unwrap();
        assert_eq!(
            validated.payload,
            json!({"state": "hi", "questions": {"q": {"type": "noul", "instructions": "x"}}})
        );
        assert_eq!(validated.required_context(300), 300);
    }

    #[test]
    fn systemone_rejects_one_option_choice_and_streaming() {
        let catalog = catalog();
        let clef = catalog.model("clef").unwrap();
        let one =
            json!({"state": "x", "questions": {"q": {"type": "choice", "instructions": "x", "criteria": {"a": null}}}});
        assert_eq!(validate(Operation::Systemone, clef, body(one)).unwrap_err().code, "invalid_request");
        let streaming =
            json!({"stream": true, "state": "x", "questions": {"q": {"type": "noul", "instructions": "x"}}});
        assert_eq!(validate(Operation::Systemone, clef, body(streaming)).unwrap_err().code, "unsupported_streaming");
    }

    #[test]
    fn h2o_takes_one_option_questions_and_noul_aliases_but_no_more_than_16() {
        let catalog = catalog();
        let h2o = catalog.model("h2o-q8").unwrap();
        let ok = json!({"state": {"a": 1}, "questions": {
            "one": {"type": "choice", "instructions": "x", "criteria": {"only": null}},
            "yes": {"type": "noul", "instructions": "x", "criteria": {"yes": "it is", "no": "it is not"}},
            "lvl": {"type": "score", "instructions": "x", "criteria": ["low"]}
        }});
        assert!(validate(Operation::Systemone, h2o, body(ok)).is_ok());
        let options: serde_json::Map<String, Value> = (0..17).map(|i| (format!("o{i}"), Value::Null)).collect();
        let many =
            json!({"state": "x", "questions": {"q": {"type": "choice", "instructions": "x", "criteria": options}}});
        assert_eq!(validate(Operation::Systemone, h2o, body(many)).unwrap_err().code, "invalid_request");
        let clef = catalog.model("clef").unwrap();
        let alias =
            json!({"state": "x", "questions": {"q": {"type": "noul", "instructions": "x", "criteria": {"yes": "y"}}}});
        assert_eq!(validate(Operation::Systemone, clef, body(alias)).unwrap_err().code, "invalid_request");
    }

    #[test]
    fn chat_reserves_output_and_forces_usage_on_streams() {
        let catalog = catalog();
        let qwen = catalog.model("qwen2.5-1.5b-instruct").unwrap();
        let validated = validate(
            Operation::ChatCompletions,
            qwen,
            body(json!({"model": "qwen2.5-1.5b-instruct", "messages": [{"role": "user", "content": "hi"}], "stream": true})),
        )
        .unwrap();
        assert!(!validated.include_usage);
        assert_eq!(validated.payload["stream_options"], json!({"include_usage": true}));
        assert_eq!(validated.output_tokens, 512);
        assert_eq!(validated.required_context(100), 613);
    }

    #[test]
    fn chat_rejects_tools_and_wrong_endpoint() {
        let catalog = catalog();
        let qwen = catalog.model("qwen2.5-1.5b-instruct").unwrap();
        let tools = json!({"messages": [{"role": "user", "content": "hi"}], "tools": []});
        assert_eq!(validate(Operation::ChatCompletions, qwen, body(tools)).unwrap_err().code, "unsupported_parameter");
        let clef = catalog.model("clef").unwrap();
        let chat = json!({"messages": [{"role": "user", "content": "hi"}]});
        assert_eq!(validate(Operation::ChatCompletions, clef, body(chat)).unwrap_err().code, "unsupported_capability");
    }
}
