//! Python `json.dumps` formatting used by the pinned H2O text contract, ported
//! from refactor-tool's `lightning_json.rs`.
use serde_json::Value;

pub enum Layout {
    Compact,
    Description,
    Instructions,
}

pub fn render(value: &Value, layout: Layout) -> String {
    let mut out = String::new();
    write(value, &layout, 0, &mut out);
    out
}

fn quoted(text: &str, ascii: bool) -> String {
    let encoded = serde_json::to_string(text).expect("JSON string serialization");
    if !ascii {
        return encoded;
    }
    let mut out = String::new();
    for c in encoded.chars() {
        if c.is_ascii() && c != '\u{7f}' {
            out.push(c);
        } else {
            for unit in c.encode_utf16(&mut [0; 2]) {
                use std::fmt::Write;
                write!(out, "\\u{unit:04x}").expect("String formatting");
            }
        }
    }
    out
}

fn number(value: &serde_json::Number) -> String {
    if !value.is_f64() {
        return value.to_string();
    }
    super::float::render(value.as_f64().expect("JSON float"))
}

fn write(value: &Value, layout: &Layout, depth: usize, out: &mut String) {
    let pretty = matches!(layout, Layout::Instructions);
    let ascii = pretty;
    match value {
        Value::String(s) => out.push_str(&quoted(s, ascii)),
        Value::Number(n) => out.push_str(&number(n)),
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                separator(i, layout, depth + 1, out);
                write(item, layout, depth + 1, out);
            }
            close(!items.is_empty(), pretty, depth, ']', out);
        }
        Value::Object(items) => {
            out.push('{');
            // serde_json's preserve_order feature retains the supplied order.
            for (i, (key, item)) in items.iter().enumerate() {
                separator(i, layout, depth + 1, out);
                out.push_str(&quoted(key, ascii));
                out.push(':');
                if !matches!(layout, Layout::Compact) {
                    out.push(' ');
                }
                write(item, layout, depth + 1, out);
            }
            close(!items.is_empty(), pretty, depth, '}', out);
        }
    }
}

fn separator(index: usize, layout: &Layout, depth: usize, out: &mut String) {
    if index > 0 {
        out.push(',');
        if matches!(layout, Layout::Description) {
            out.push(' ');
        }
    }
    if matches!(layout, Layout::Instructions) {
        out.push('\n');
        out.push_str(&" ".repeat(depth));
    }
}

fn close(nonempty: bool, pretty: bool, depth: usize, end: char, out: &mut String) {
    if nonempty && pretty {
        out.push('\n');
        out.push_str(&" ".repeat(depth));
    }
    out.push(end);
}
