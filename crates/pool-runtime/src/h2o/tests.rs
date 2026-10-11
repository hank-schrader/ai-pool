use serde_json::json;

use super::{
    engine, float,
    json::{self, Layout},
    prompt,
};

#[test]
fn floats_print_like_python_repr() {
    for (value, text) in [
        (0.1, "0.1"),
        (100.0, "100.0"),
        (-0.0, "-0.0"),
        (1e16, "1e+16"),
        (1.5e-5, "1.5e-05"),
        (0.0001, "0.0001"),
        (123456789012345680.0, "1.2345678901234568e+17"),
        (f64::INFINITY, "Infinity"),
    ] {
        assert_eq!(float::render(value), text);
    }
}

#[test]
fn json_layouts_match_python_dumps() {
    let value = json!({"a": [1, 2.5, "\u{e9}"], "b": null, "c": {}});
    assert_eq!(json::render(&value, Layout::Compact), r#"{"a":[1,2.5,"é"],"b":null,"c":{}}"#);
    assert_eq!(json::render(&value, Layout::Description), r#"{"a": [1, 2.5, "é"], "b": null, "c": {}}"#);
    assert_eq!(
        json::render(&json!({"a": [1], "b": "\u{e9}"}), Layout::Instructions),
        "{\n \"a\": [\n  1\n ],\n \"b\": \"\\u00e9\"\n}"
    );
}

#[test]
fn prompt_is_the_chatml_answer_slot_text() {
    let question = json!({
        "type": "choice",
        "instructions": "  Which team should handle this?",
        "criteria": {"billing": null, "shipping": "parcels, \"late\" ones"}
    });
    let text = prompt::render(&json!("Customer: charged twice."), &question).unwrap();
    assert_eq!(
        text,
        "<|im_start|>system\nYou are a decision engine. You read a record and answer one question about it by \
         choosing exactly one option. Reply with a single letter.<|im_end|>\n<|im_start|>user\nrecord: Customer: \
         charged twice.\nquestion: Which team should handle this?\noptions:\nA) billing: billing\nB) shipping: \
         parcels, \"late\" ones<|im_end|>\n<|im_start|>assistant\n<think>\n\n</think>\n\nAnswer:"
    );
}

#[test]
fn noul_and_score_options_follow_the_original() {
    let (names, descriptions) = prompt::options(&json!({"type": "noul", "instructions": "x"})).unwrap();
    assert_eq!(names, ["true", "false"]);
    assert_eq!(descriptions, ["the statement holds", "it does not"]);
    let (names, _) = prompt::options(&json!({"type": "score", "criteria": ["low", "high"]})).unwrap();
    assert_eq!(names, ["0", "1"]);
    let too_many = json!({"type": "score", "criteria": vec!["x"; 17]});
    assert!(prompt::options(&too_many).is_err());
    assert!(prompt::options(&json!({"type": "ranking"})).is_err());
}

#[test]
fn probabilities_and_noul_commit() {
    let p = engine::probabilities(&[0.9_f64.ln(), 0.1_f64.ln()], 0.8);
    assert!((p.iter().sum::<f64>() - 1.0).abs() < 1e-12);
    assert!(p[0] > 0.9);
    assert_eq!(engine::commit_noul(0.95), 0.95);
    assert_eq!(engine::commit_noul(0.65), 0.801);
    assert!((engine::commit_noul(0.4) - 0.199).abs() < 1e-12);
    assert_eq!(engine::commit_noul(0.5), 0.801);
}

#[test]
fn completion_body_biases_only_the_labels() {
    let body = String::from_utf8(engine::completion_body(&[5, 6], &[10, 11])).unwrap();
    let value: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(value["prompt"], json!([5, 6]));
    assert_eq!(value["logit_bias"], json!([[10, 64.0], [11, 64.0]]));
    assert_eq!(value["n_probs"], 2);
    assert_eq!(value["temperature"], 1.0);
    assert!(body.contains("\"temperature\":1.0,"));
}
