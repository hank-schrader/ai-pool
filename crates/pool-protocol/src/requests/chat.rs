use super::*;
use options::{check_message, check_sampling, output_tokens};

const CHAT_PASSTHROUGH: &[&str] = &[
    "temperature",
    "top_p",
    "top_k",
    "min_p",
    "repeat_penalty",
    "stop",
    "frequency_penalty",
    "presence_penalty",
    "seed",
];
const CHAT_UNSUPPORTED: &[&str] = &[
    "tools",
    "tool_choice",
    "functions",
    "function_call",
    "parallel_tool_calls",
    "logprobs",
    "top_logprobs",
    "logit_bias",
    "audio",
    "modalities",
    "prediction",
    "reasoning_effort",
    "web_search_options",
];

pub(super) fn validate(model: &Model, mut body: Map<String, Value>) -> Result<Validated, RequestError> {
    let routing = Routing::take(&mut body)?;
    let min_context_tokens = min_context_tokens(model, &mut body)?;
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
    options::structured(&mut body, &mut payload)?;
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
        question_count: 1,
        stream,
        include_usage,
        output_tokens,
        min_context_tokens,
        routing,
        count_payload: Value::Object(payload.clone()),
        payload: Value::Object(payload),
    })
}
