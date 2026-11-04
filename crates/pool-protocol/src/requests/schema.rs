//! Bound work in the pinned runtime's schema builder and grammar compiler.
//! This is a safety check, not a replacement for runtime schema semantics.

mod pattern;

use std::collections::HashSet;

use super::{RequestError, Value};

const MAX_DEPTH: usize = 32;
const MAX_NODES: usize = 2048;
const MAX_EXPANSION: usize = 32_768;
const MAX_EXPANDED_DATA_BYTES: usize = 1 << 20;
// The pinned llama.cpp grammar parser's repetition limit.
const MAX_REPEAT: u64 = 2000;

fn invalid(path: &str, message: &str) -> RequestError {
    RequestError::invalid(message, Some(path.into()))
}

pub(super) fn validate(schema: &Value, path: &str) -> Result<(), RequestError> {
    if !schema.is_object() {
        return Err(invalid(path, "schema must be an object"));
    }
    // Include annotation/literal data, too: it is cloned for type alternatives.
    document(schema, path, 0, &mut 8192)?;
    Validator {
        root: schema,
        nodes: MAX_NODES,
        expansion: MAX_EXPANSION,
        data_bytes: MAX_EXPANDED_DATA_BYTES,
        properties: 0,
        active: HashSet::new(),
    }
    .visit(schema, path, 0, 1)
}

fn document(value: &Value, path: &str, depth: usize, remaining: &mut usize) -> Result<(), RequestError> {
    if depth > 64 || *remaining == 0 {
        return Err(invalid(path, "schema document exceeds depth or node limit"));
    }
    *remaining -= 1;
    match value {
        Value::Object(fields) => {
            for (key, value) in fields {
                if key.len() > 128 {
                    return Err(invalid(path, "schema keys must be at most 128 bytes"));
                }
                document(value, path, depth + 1, remaining)?;
            }
        }
        Value::Array(values) => {
            for value in values {
                document(value, path, depth + 1, remaining)?;
            }
        }
        Value::String(s) if s.len() > 8192 => return Err(invalid(path, "schema strings must be at most 8192 bytes")),
        _ => {}
    }
    Ok(())
}

struct Validator<'a> {
    root: &'a Value,
    nodes: usize,
    expansion: usize,
    data_bytes: usize,
    properties: usize,
    active: HashSet<*const Value>,
}

impl Validator<'_> {
    fn charge(&mut self, amount: Option<usize>, path: &str) -> Result<(), RequestError> {
        self.expansion = amount
            .and_then(|n| self.expansion.checked_sub(n))
            .ok_or_else(|| invalid(path, "schema exceeds compilation expansion limit"))?;
        Ok(())
    }

    fn charge_data(&mut self, value: &Value, copies: usize, path: &str) -> Result<(), RequestError> {
        let bytes = serde_json::to_vec(value).expect("JSON value serializes").len();
        self.data_bytes = bytes
            .checked_mul(copies)
            .and_then(|n| self.data_bytes.checked_sub(n))
            .ok_or_else(|| invalid(path, "schema exceeds expanded literal or type-copy byte limit"))?;
        Ok(())
    }

    fn visit(&mut self, schema: &Value, path: &str, depth: usize, copies: usize) -> Result<(), RequestError> {
        if depth > MAX_DEPTH || self.nodes == 0 {
            return Err(invalid(path, "schema exceeds depth or node limit"));
        }
        self.nodes -= 1;
        if !self.active.insert(schema as *const Value) {
            return Err(invalid(path, "recursive schema references cannot be safely bounded"));
        }
        let result = self.node(schema, path, depth, copies);
        self.active.remove(&(schema as *const Value));
        result
    }

    fn node(&mut self, schema: &Value, path: &str, depth: usize, copies: usize) -> Result<(), RequestError> {
        if schema.is_boolean() {
            return self.charge(Some(copies), path);
        }
        let fields = schema.as_object().ok_or_else(|| invalid(path, "expected a schema object or boolean"))?;
        let types = match fields.get("type") {
            None => 1,
            Some(Value::String(kind)) if supported(kind) => 1,
            Some(Value::Array(types)) if !types.is_empty() => {
                let mut seen = HashSet::new();
                for kind in types {
                    let kind = kind
                        .as_str()
                        .filter(|kind| supported(kind))
                        .ok_or_else(|| invalid(&format!("{path}.type"), "expected supported type strings"))?;
                    if !seen.insert(kind) {
                        return Err(invalid(&format!("{path}.type"), "type alternatives must be unique"));
                    }
                }
                types.len()
            }
            Some(_) => {
                return Err(invalid(&format!("{path}.type"), "expected a supported type or nonempty type array"));
            }
        };
        let copies = copies.checked_mul(types).ok_or_else(|| invalid(path, "schema type expansion overflow"))?;
        self.charge(Some(copies), path)?;
        if types > 1 {
            // The builder clones the whole schema, including annotations, for
            // every type alternative before recursively compiling its children.
            self.charge_data(schema, copies, path)?;
        }
        // Charge repetitions as well as nodes; valid integer bounds alone do not
        // bound the grammar rules allocated when a sampler is initialized.
        for (min, max) in [
            ("minLength", "maxLength"),
            ("minItems", "maxItems"),
            ("minProperties", "maxProperties"),
            ("minContains", "maxContains"),
        ] {
            let low = count(fields.get(min), &format!("{path}.{min}"))?.unwrap_or(0);
            let high = count(fields.get(max), &format!("{path}.{max}"))?;
            if high.is_some_and(|high| low > high) {
                return Err(invalid(&format!("{path}.{max}"), "maximum count must be at least the minimum"));
            }
            self.charge(copies.checked_mul(high.unwrap_or(low) as usize), path)?;
        }
        if let Some(value) = fields.get("pattern") {
            let at = format!("{path}.pattern");
            let value = value.as_str().ok_or_else(|| invalid(&at, "pattern must be a string"))?;
            self.charge(copies.checked_mul(pattern::cost(value, &at)?), &at)?;
        }
        if let Some(value) = fields.get("$ref") {
            let at = format!("{path}.$ref");
            let reference = value
                .as_str()
                .filter(|s| s.starts_with("#/"))
                .ok_or_else(|| invalid(&at, "only local schema references starting with #/ are supported"))?;
            // The pinned builder does not decode pointer escapes or URI escapes.
            if reference.contains(['~', '%']) {
                return Err(invalid(&at, "escaped schema references cannot be safely resolved by the runtime"));
            }
            let target =
                self.root.pointer(&reference[1..]).ok_or_else(|| invalid(&at, "schema reference does not resolve"))?;
            self.visit(target, &format!("{path}.$ref({reference})"), depth + 1, copies)?;
        }
        for (key, value) in fields {
            let at = format!("{path}.{key}");
            match key.as_str() {
                "properties" | "patternProperties" | "$defs" | "definitions" | "dependentSchemas" | "dependencies" => {
                    let children = value.as_object().ok_or_else(|| invalid(&at, "expected a map of schemas"))?;
                    // Optional-property generation is quadratic in the property count.
                    if key == "properties" {
                        let properties = copies.checked_mul(children.len());
                        self.charge(properties.and_then(|n| n.checked_mul(children.len())), &at)?;
                        self.properties = properties
                            .and_then(|n| self.properties.checked_add(n))
                            .ok_or_else(|| invalid(&at, "schema property expansion overflow"))?;
                    }
                    for (name, child) in children {
                        let child_at = format!("{at}.{name}");
                        if key == "dependencies" && child.is_array() {
                            continue;
                        }
                        self.visit(child, &child_at, depth + 1, copies)?;
                    }
                }
                "allOf" | "anyOf" | "oneOf" => {
                    let children = value
                        .as_array()
                        .filter(|v| !v.is_empty())
                        .ok_or_else(|| invalid(&at, "expected a nonempty array of schemas"))?;
                    let before = self.properties;
                    for (index, child) in children.iter().enumerate() {
                        self.visit(child, &format!("{at}[{index}]"), depth + 1, copies)?;
                    }
                    if key == "allOf" {
                        // The converter merges properties from referenced and
                        // alternative objects into one optional-property rule.
                        let properties = self.properties - before;
                        self.charge(properties.checked_mul(properties), &at)?;
                    }
                }
                "items" | "prefixItems" => {
                    if let Some(children) = value.as_array() {
                        for (index, child) in children.iter().enumerate() {
                            self.visit(child, &format!("{at}[{index}]"), depth + 1, copies)?;
                        }
                    } else {
                        self.visit(value, &at, depth + 1, copies)?;
                    }
                }
                "additionalProperties"
                | "additionalItems"
                | "unevaluatedProperties"
                | "unevaluatedItems"
                | "contains"
                | "propertyNames"
                | "not"
                | "if"
                | "then"
                | "else"
                | "contentSchema" => {
                    self.visit(value, &at, depth + 1, copies)?;
                }
                "enum" => {
                    let values = value
                        .as_array()
                        .filter(|v| !v.is_empty())
                        .ok_or_else(|| invalid(&at, "enum must be a nonempty array"))?;
                    self.charge(copies.checked_mul(values.len()), &at)?;
                    self.charge_data(value, copies, &at)?;
                }
                "const" => self.charge_data(value, copies, &at)?,
                _ => {}
            }
        }
        Ok(())
    }
}

fn count(value: Option<&Value>, path: &str) -> Result<Option<u64>, RequestError> {
    value
        .map(|value| {
            value
                .as_u64()
                .filter(|n| *n <= MAX_REPEAT)
                .ok_or_else(|| invalid(path, "schema counts must be nonnegative integers at most 2000"))
        })
        .transpose()
}

fn supported(kind: &str) -> bool {
    matches!(kind, "object" | "array" | "string" | "number" | "integer" | "boolean" | "null")
}
