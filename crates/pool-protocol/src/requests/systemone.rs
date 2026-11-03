use super::*;

pub(super) fn validate(model: &Model, mut body: Map<String, Value>) -> Result<Validated, RequestError> {
    let min_context_tokens = min_context_tokens(model, &mut body)?;
    let limits = &model.limits;
    let (max_questions, max_options, max_levels) = (
        limits.max_questions.unwrap_or(16),
        limits.max_choice_options.unwrap_or(16),
        limits.max_score_levels.unwrap_or(10),
    );
    // H2O accepts one-option questions (labels A-P) and the "yes"/"no" aliases of
    // the noul sides, as refactor-tool's engine does
    let h2o = model.backend == Backend::LlamaServerH2o;
    let min_options = if h2o { 1 } else { 2 };
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
                Some(Value::Object(options)) if (min_options..=max_options).contains(&options.len()) => {
                    if options.keys().any(String::is_empty) {
                        return Err(RequestError::invalid("choice option keys must not be empty", criteria_at));
                    }
                }
                _ => {
                    return Err(RequestError::invalid(
                        format!("a choice question needs \"criteria\" with {min_options}-{max_options} options"),
                        criteria_at,
                    ));
                }
            },
            Some("score") => match criteria {
                Some(Value::Array(levels)) if (min_options..=max_levels).contains(&levels.len()) => {}
                _ => {
                    return Err(RequestError::invalid(
                        format!(
                            "a score question needs \"criteria\" with {min_options}-{max_levels} levels, lowest first"
                        ),
                        criteria_at,
                    ));
                }
            },
            Some("noul") => match criteria {
                None => {}
                Some(Value::Object(sides))
                    if sides.keys().all(|key| {
                        matches!(key.as_str(), "true" | "false") || (h2o && matches!(key.as_str(), "yes" | "no"))
                    }) => {}
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
    let question_count = questions.len() as u64;
    let payload = json!({ "state": state, "questions": questions });
    Ok(Validated {
        operation: Operation::Systemone,
        question_count,
        stream: false,
        include_usage: false,
        output_tokens: 0,
        min_context_tokens,
        routing: Routing::default(),
        count_payload: payload.clone(),
        payload,
    })
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
