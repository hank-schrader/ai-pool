//! The H2O shim 672dc01e prompt (text only, wrapper 0, original option order),
//! ported from refactor-tool's `lightning_prompt.rs`.
use pool_protocol::messages::JobErrorCode;
use serde_json::Value;

use super::json::{self, Layout};
use crate::RuntimeError;

/// The public API accepts at most 16 options: the prefix of the shim's ordered
/// label list, not caller keys.
pub const LABELS: [&str; 16] = ["A", "B", "C", "D", "E", "F", "G", "H", "I", "J", "K", "L", "M", "N", "O", "P"];
const SYSTEM: &str = "You are a decision engine. You read a record and answer one question about it by choosing exactly one option. Reply with a single letter.";

fn invalid(message: impl Into<String>) -> RuntimeError {
    RuntimeError::new(JobErrorCode::InvalidRequest, message)
}

fn whitespace(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

fn text(value: &Value, layout: Layout) -> String {
    value.as_str().map(str::to_owned).unwrap_or_else(|| json::render(value, layout))
}

/// Option names (the response keys) and their descriptions, in caller order.
pub fn options(question: &Value) -> Result<(Vec<String>, Vec<String>), RuntimeError> {
    let criteria = &question["criteria"];
    let (names, descriptions) = match question["type"].as_str().unwrap_or("choice") {
        "noul" => {
            let description = |key: &str, alias: &str, default: &str| {
                let value = criteria.get(key).or_else(|| criteria.get(alias)).unwrap_or(&Value::Null);
                if value.is_null() || value.as_str() == Some("") {
                    default.to_owned()
                } else {
                    text(value, Layout::Description)
                }
            };
            (
                vec!["true".into(), "false".into()],
                vec![description("true", "yes", "the statement holds"), description("false", "no", "it does not")],
            )
        }
        "score" => {
            let levels = criteria.as_array().ok_or_else(|| invalid("score requires levels"))?;
            (
                (0..levels.len()).map(|i| i.to_string()).collect(),
                levels.iter().map(|v| text(v, Layout::Description)).collect(),
            )
        }
        "choice" => {
            let choices = criteria.as_object().ok_or_else(|| invalid("choice requires criteria"))?;
            (
                choices.keys().cloned().collect(),
                choices
                    .iter()
                    .map(|(k, v)| if v.is_null() { k.clone() } else { text(v, Layout::Description) })
                    .collect(),
            )
        }
        other => return Err(invalid(format!("unknown H2O question type {other}"))),
    };
    if names.is_empty() || names.len() > LABELS.len() {
        return Err(invalid("H2O public API requires 1-16 options"));
    }
    Ok((names, descriptions))
}

/// The full ChatML prompt for one question, labels A.. in caller order.
pub fn render(state: &Value, question: &Value) -> Result<String, RuntimeError> {
    let (names, descriptions) = options(question)?;
    let instructions =
        text(question.get("instructions").unwrap_or(&Value::String(String::new())), Layout::Instructions);
    let instructions = instructions.trim_start_matches(whitespace);
    let instructions = if instructions.is_empty() { "Answer the question below." } else { instructions };
    let lines = names
        .iter()
        .zip(&descriptions)
        .zip(LABELS)
        .map(|((name, desc), label)| format!("{label}) {name}: {desc}"))
        .collect::<Vec<_>>()
        .join("\n");
    let user = format!("record: {}\nquestion: {instructions}\noptions:\n{lines}", text(state, Layout::Compact));
    Ok(format!(
        "<|im_start|>system\n{SYSTEM}<|im_end|>\n<|im_start|>user\n{}<|im_end|>\n<|im_start|>assistant\n<think>\n\n</think>\n\nAnswer:",
        user.trim_matches(whitespace)
    ))
}

/// A 16-option prompt for the startup label check.
pub fn probe() -> String {
    let criteria =
        (0..LABELS.len()).map(|i| (format!("o{i}"), Value::String(format!("d{i}")))).collect::<serde_json::Map<_, _>>();
    render(
        &serde_json::json!({"x": 1}),
        &serde_json::json!({"type": "choice", "instructions": "Which?", "criteria": criteria}),
    )
    .expect("pinned label probe")
}
