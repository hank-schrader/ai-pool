//! Client request validation. The pool forwards only fields it understands;
//! anything it would silently ignore is rejected instead.

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
    pub stream: bool,
    /// Client asked for the final usage chunk of a stream.
    pub include_usage: bool,
    /// Tokens reserved for output on top of the counted input.
    pub output_tokens: u32,
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
        (Operation::Systemone, Backend::LlamaServerSystemone) => systemone(model, body),
        (Operation::ChatCompletions, Backend::LlamaServerChat) => chat(model, body),
        _ => Err(RequestError {
            code: "unsupported_capability",
            message: format!(
                "model {:?} serves {}; use its endpoint instead",
                model.id,
                match model.backend {
                    Backend::LlamaServerSystemone => "POST /v1/systemone",
                    Backend::LlamaServerChat => "POST /v1/chat/completions",
                }
            ),
            param: Some("model".into()),
        }),
    }
}

fn systemone(model: &Model, mut body: Map<String, Value>) -> Result<Validated, RequestError> {
    let limits = &model.limits;
    let (max_questions, max_options, max_levels) = (
        limits.max_questions.unwrap_or(16),
        limits.max_choice_options.unwrap_or(16),
        limits.max_score_levels.unwrap_or(10),
    );
    body.remove("model");
    if body.remove("stream").is_some_and(|stream| stream != Value::Bool(false)) {
        return Err(RequestError {
            code: "unsupported_streaming",
            message: "systemone does not stream; omit \"stream\"".into(),
            param: Some("stream".into()),
        });
    }
    if body.contains_key("images") {
        return Err(RequestError::unsupported("images", "image input is not supported for this model"));
    }
    let state = body.remove("state").filter(|state| !state.is_null());
    let Some(state) = state else {
        return Err(RequestError::invalid("\"state\" must be provided", Some("state".into())));
    };
    let questions = match body.remove("questions") {
        Some(Value::Object(questions)) => questions,
        _ => return Err(RequestError::invalid("\"questions\" must be an object", Some("questions".into()))),
    };
    if let Some(key) = body.keys().next() {
        return Err(RequestError::unsupported(key, format!("unknown field {key:?}")));
    }
    if questions.is_empty() || questions.len() > max_questions {
        return Err(RequestError::invalid(
            format!("between 1 and {max_questions} questions are allowed"),
            Some("questions".into()),
        ));
    }
    for (id, question) in &questions {
        let at = format!("questions.{id}");
        if id.is_empty() || id.len() > 128 {
            return Err(RequestError::invalid("question ids must be 1-128 characters", Some(at)));
        }
        let Value::Object(question) = question else {
            return Err(RequestError::invalid("a question must be an object", Some(at)));
        };
        if let Some(key) = question.keys().find(|key| !matches!(key.as_str(), "type" | "instructions" | "criteria")) {
            return Err(RequestError::unsupported(&format!("{at}.{key}"), format!("unknown field {key:?}")));
        }
        if is_empty(question.get("instructions")) {
            return Err(RequestError::invalid(
                "\"instructions\" must not be empty",
                Some(format!("{at}.instructions")),
            ));
        }
        let criteria = question.get("criteria").filter(|value| !value.is_null());
        let criteria_at = Some(format!("{at}.criteria"));
        match question.get("type").and_then(Value::as_str) {
            Some("choice") => match criteria {
                Some(Value::Object(options)) if (2..=max_options).contains(&options.len()) => {
                    if options.keys().any(String::is_empty) {
                        return Err(RequestError::invalid("choice option keys must not be empty", criteria_at));
                    }
                }
                _ => {
                    return Err(RequestError::invalid(
                        format!("a choice question needs \"criteria\" with 2-{max_options} options"),
                        criteria_at,
                    ));
                }
            },
            Some("score") => match criteria {
                Some(Value::Array(levels)) if (2..=max_levels).contains(&levels.len()) => {}
                _ => {
                    return Err(RequestError::invalid(
                        format!("a score question needs \"criteria\" with 2-{max_levels} levels, lowest first"),
                        criteria_at,
                    ));
                }
            },
            Some("noul") => match criteria {
                None => {}
                Some(Value::Object(sides)) if sides.keys().all(|key| key == "true" || key == "false") => {}
                Some(_) => {
                    return Err(RequestError::invalid(
                        "noul \"criteria\" may only describe \"true\" and \"false\"",
                        criteria_at,
                    ));
                }
            },
            _ => {
                return Err(RequestError::invalid(
                    "\"type\" must be one of: choice, score, noul",
                    Some(format!("{at}.type")),
                ));
            }
        }
    }
    let payload = json!({ "state": state, "questions": questions });
    Ok(Validated {
        operation: Operation::Systemone,
        stream: false,
        include_usage: false,
        output_tokens: 0,
        count_payload: payload.clone(),
        payload,
    })
}

const CHAT_PASSTHROUGH: &[&str] = &["temperature", "top_p", "stop", "frequency_penalty", "presence_penalty", "seed"];
const CHAT_UNSUPPORTED: &[&str] = &[
    "tools",
    "tool_choice",
    "functions",
    "function_call",
    "parallel_tool_calls",
    "response_format",
    "logprobs",
    "top_logprobs",
    "logit_bias",
    "audio",
    "modalities",
    "prediction",
    "reasoning_effort",
    "web_search_options",
];

fn chat(model: &Model, mut body: Map<String, Value>) -> Result<Validated, RequestError> {
    let default_output = model.limits.default_output_tokens.unwrap_or(512);
    let max_output = model.limits.max_output_tokens.unwrap_or(2048);
    body.remove("model");
    // metadata only; not forwarded
    body.remove("user");

    if let Some(key) = body.keys().find(|key| CHAT_UNSUPPORTED.contains(&key.as_str())) {
        return Err(RequestError::unsupported(key, format!("{key:?} is not supported by this pool yet")));
    }

    let stream = match body.remove("stream") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(stream)) => stream,
        Some(_) => return Err(RequestError::invalid("\"stream\" must be a boolean", Some("stream".into()))),
    };
    let include_usage = match body.remove("stream_options") {
        None | Some(Value::Null) => false,
        Some(Value::Object(options)) if stream => {
            if let Some(key) = options.keys().find(|key| *key != "include_usage") {
                return Err(RequestError::unsupported(&format!("stream_options.{key}"), "unknown stream option"));
            }
            match options.get("include_usage") {
                None | Some(Value::Null) => false,
                Some(Value::Bool(include)) => *include,
                Some(_) => {
                    return Err(RequestError::invalid(
                        "include_usage must be a boolean",
                        Some("stream_options.include_usage".into()),
                    ));
                }
            }
        }
        Some(_) => {
            return Err(RequestError::invalid(
                "\"stream_options\" needs \"stream\": true",
                Some("stream_options".into()),
            ));
        }
    };

    match body.remove("n") {
        None | Some(Value::Null) => {}
        Some(n) if n == json!(1) => {}
        Some(_) => return Err(RequestError::unsupported("n", "only n = 1 is supported")),
    }

    let output_tokens = output_tokens(&mut body, default_output, max_output)?;

    let messages = match body.remove("messages") {
        Some(Value::Array(messages)) if !messages.is_empty() => messages,
        _ => return Err(RequestError::invalid("\"messages\" must be a non-empty array", Some("messages".into()))),
    };
    for (index, message) in messages.iter().enumerate() {
        check_message(index, message)?;
    }

    let mut payload = Map::new();
    payload.insert("messages".into(), Value::Array(messages.clone()));
    for key in CHAT_PASSTHROUGH {
        if let Some(value) = body.remove(*key) {
            check_sampling(key, &value)?;
            if !value.is_null() {
                payload.insert((*key).into(), value);
            }
        }
    }
    if let Some(key) = body.keys().next() {
        return Err(RequestError::unsupported(key, format!("unknown field {key:?}")));
    }
    payload.insert("max_tokens".into(), output_tokens.into());
    payload.insert("stream".into(), stream.into());
    if stream {
        // the miner always asks for usage and the pool decides whether to relay it
        payload.insert("stream_options".into(), json!({ "include_usage": true }));
    }

    Ok(Validated {
        operation: Operation::ChatCompletions,
        stream,
        include_usage,
        output_tokens,
        count_payload: json!({ "messages": messages }),
        payload: Value::Object(payload),
    })
}

fn output_tokens(body: &mut Map<String, Value>, default: u32, max: u32) -> Result<u32, RequestError> {
    let read = |key: &str, value: Option<Value>| -> Result<Option<u32>, RequestError> {
        match value {
            None | Some(Value::Null) => Ok(None),
            Some(value) => match value.as_u64() {
                Some(n) if n >= 1 && n <= max as u64 => Ok(Some(n as u32)),
                _ => Err(RequestError::invalid(format!("{key} must be an integer from 1 to {max}"), Some(key.into()))),
            },
        }
    };
    let max_tokens = read("max_tokens", body.remove("max_tokens"))?;
    let completion = read("max_completion_tokens", body.remove("max_completion_tokens"))?;
    match (max_tokens, completion) {
        (Some(a), Some(b)) if a != b => Err(RequestError::invalid(
            "max_tokens and max_completion_tokens disagree",
            Some("max_completion_tokens".into()),
        )),
        (a, b) => Ok(a.or(b).unwrap_or(default)),
    }
}

fn check_message(index: usize, message: &Value) -> Result<(), RequestError> {
    let at = format!("messages[{index}]");
    let Value::Object(message) = message else {
        return Err(RequestError::invalid("a message must be an object", Some(at)));
    };
    match message.get("role").and_then(Value::as_str) {
        Some("system" | "developer" | "user" | "assistant") => {}
        Some("tool" | "function") => {
            return Err(RequestError::unsupported(&format!("{at}.role"), "tool messages are not supported"));
        }
        _ => {
            return Err(RequestError::invalid(
                "role must be system, developer, user or assistant",
                Some(format!("{at}.role")),
            ));
        }
    }
    if let Some(key) = message.keys().find(|key| !matches!(key.as_str(), "role" | "content" | "name")) {
        return Err(RequestError::unsupported(&format!("{at}.{key}"), format!("{key:?} is not supported in messages")));
    }
    match message.get("content") {
        Some(Value::String(_)) => Ok(()),
        Some(Value::Array(parts)) => {
            for (part_index, part) in parts.iter().enumerate() {
                let part_at = format!("{at}.content[{part_index}]");
                match part.get("type").and_then(Value::as_str) {
                    Some("text") if part.get("text").is_some_and(Value::is_string) => {}
                    Some("text") => {
                        return Err(RequestError::invalid("a text part needs a string \"text\"", Some(part_at)));
                    }
                    _ => return Err(RequestError::unsupported(&part_at, "only text content parts are supported")),
                }
            }
            Ok(())
        }
        _ => Err(RequestError::invalid(
            "content must be a string or an array of text parts",
            Some(format!("{at}.content")),
        )),
    }
}

fn check_sampling(key: &str, value: &Value) -> Result<(), RequestError> {
    let ok = match key {
        "stop" => match value {
            Value::Null | Value::String(_) => true,
            Value::Array(items) => items.len() <= 4 && items.iter().all(Value::is_string),
            _ => false,
        },
        "seed" => value.is_null() || value.is_i64() || value.is_u64(),
        "temperature" => value.is_null() || value.as_f64().is_some_and(|v| (0.0..=2.0).contains(&v)),
        "top_p" => value.is_null() || value.as_f64().is_some_and(|v| (0.0..=1.0).contains(&v)),
        _ => value.is_null() || value.as_f64().is_some_and(|v| (-2.0..=2.0).contains(&v)),
    };
    if ok { Ok(()) } else { Err(RequestError::invalid(format!("invalid {key}"), Some(key.into()))) }
}

fn is_empty(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => true,
        Some(Value::String(text)) => text.trim().is_empty(),
        Some(Value::Array(items)) => items.is_empty(),
        Some(Value::Object(fields)) => fields.is_empty(),
        Some(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::Catalog;

    fn catalog() -> Catalog {
        Catalog::parse(include_str!("../../../config/models.json")).unwrap()
    }

    fn body(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
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
