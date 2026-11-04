use super::*;

pub(super) fn structured(body: &mut Map<String, Value>, payload: &mut Map<String, Value>) -> Result<(), RequestError> {
    if let Some(format) = body.remove("response_format") {
        let invalid = || {
            RequestError::invalid(
                "expected text, json_object, or json_schema with a schema object",
                Some("response_format".into()),
            )
        };
        let object = format.as_object().ok_or_else(invalid)?;
        let allowed: &[&str] = match object.get("type").and_then(Value::as_str) {
            Some("text") => &["type"],
            Some("json_object") => {
                if let Some(schema) = object.get("schema") {
                    super::schema::validate(schema, "response_format.schema")?;
                }
                &["type", "schema"]
            }
            Some("json_schema") => {
                let schema = object.get("json_schema").and_then(Value::as_object).ok_or_else(invalid)?;
                if !schema.contains_key("schema")
                    || schema.keys().any(|k| !matches!(k.as_str(), "name" | "strict" | "schema"))
                    || schema.get("name").is_some_and(|v| !v.as_str().is_some_and(|s| !s.is_empty() && s.len() <= 64))
                    || schema.get("strict").is_some_and(|v| !v.is_boolean())
                {
                    return Err(invalid());
                }
                super::schema::validate(&schema["schema"], "response_format.json_schema.schema")?;
                &["type", "json_schema"]
            }
            _ => return Err(invalid()),
        };
        if object.keys().any(|key| !allowed.contains(&key.as_str())) {
            return Err(invalid());
        }
        payload.insert("response_format".into(), format);
    }
    if let Some(kwargs) = body.remove("chat_template_kwargs") {
        let valid = kwargs.as_object().is_some_and(|fields| {
            fields.len() <= 16
                && fields.iter().all(|(key, value)| {
                    !key.is_empty()
                        && key.len() <= 64
                        && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                        && match key.as_str() {
                            "enable_thinking" | "preserve_thinking" => value.is_boolean(),
                            _ => {
                                value.is_boolean()
                                    || value.is_number()
                                    || value.is_null()
                                    || value.as_str().is_some_and(|s| s.len() <= 1024)
                            }
                        }
                })
        });
        if !valid {
            return Err(RequestError::invalid(
                "bounded scalar template kwargs required",
                Some("chat_template_kwargs".into()),
            ));
        }
        payload.insert("chat_template_kwargs".into(), kwargs);
    }
    Ok(())
}

pub(super) fn output_tokens(body: &mut Map<String, Value>, default: u32, max: u32) -> Result<u32, RequestError> {
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

pub(super) fn check_message(index: usize, message: &Value) -> Result<(), RequestError> {
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

pub(super) fn check_sampling(key: &str, value: &Value) -> Result<(), RequestError> {
    let ok = match key {
        "stop" => match value {
            Value::Null | Value::String(_) => true,
            Value::Array(items) => items.len() <= 4 && items.iter().all(Value::is_string),
            _ => false,
        },
        "seed" => value.is_null() || value.as_i64().is_some_and(|v| (-1..=u32::MAX as i64).contains(&v)),
        "temperature" => value.is_null() || value.as_f64().is_some_and(|v| (0.0..=2.0).contains(&v)),
        "top_k" => value.is_null() || value.as_u64().is_some_and(|n| n <= i32::MAX as u64),
        "repeat_penalty" => value.is_null() || value.as_f64().is_some_and(|v| (0.0..=10.0).contains(&v)),
        "min_p" | "top_p" => value.is_null() || value.as_f64().is_some_and(|v| (0.0..=1.0).contains(&v)),
        _ => value.is_null() || value.as_f64().is_some_and(|v| (-2.0..=2.0).contains(&v)),
    };
    if ok { Ok(()) } else { Err(RequestError::invalid(format!("invalid {key}"), Some(key.into()))) }
}
