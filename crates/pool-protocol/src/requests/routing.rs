use super::*;

#[derive(Debug, Clone, Default)]
pub struct Routing {
    pub runtime_fingerprint: Option<String>,
    pub max_attempts: Option<u32>,
}

impl Routing {
    pub(super) fn take(body: &mut Map<String, Value>) -> Result<Self, RequestError> {
        let Some(value) = body.remove("pool") else { return Ok(Self::default()) };
        let Value::Object(mut fields) = value else {
            return Err(RequestError::invalid("pool must be an object", Some("pool".into())));
        };
        let runtime_fingerprint = match fields.remove("runtime_fingerprint") {
            None => None,
            Some(Value::String(pin))
                if pin.starts_with("sha256:")
                    && pin.len() == 71
                    && pin[7..].bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) =>
            {
                Some(pin)
            }
            Some(_) => {
                return Err(RequestError::invalid(
                    "expected sha256 fingerprint",
                    Some("pool.runtime_fingerprint".into()),
                ));
            }
        };
        let max_attempts = match fields.remove("max_attempts") {
            None => None,
            Some(value) if value == json!(1) || value == json!(2) => Some(value.as_u64().unwrap() as u32),
            Some(_) => {
                return Err(RequestError::invalid("max_attempts must be 1 or 2", Some("pool.max_attempts".into())));
            }
        };
        if let Some(key) = fields.keys().next() {
            return Err(RequestError::unsupported(&format!("pool.{key}"), "unknown routing control"));
        }
        Ok(Self { runtime_fingerprint, max_attempts })
    }
}
