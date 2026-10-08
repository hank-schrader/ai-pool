//! OpenAI-shaped error responses with pool fields (`request_id`, `retryable`).

use axum::{
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use pool_protocol::requests::RequestError;
use serde_json::{Value, json};

#[derive(Debug, Clone)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
    pub param: Option<String>,
    pub retry_after: Option<u64>,
    pub request_id: Option<String>,
}

impl ApiError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self { status, code, message: message.into(), param: None, retry_after: None, request_id: None }
    }

    pub fn unauthorized() -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "unauthorized", "missing or invalid credentials")
    }

    pub fn model_not_found(model: &str) -> Self {
        Self::new(StatusCode::NOT_FOUND, "model_not_found", format!("model {model:?} is not in this pool's catalog"))
            .param("model")
    }

    pub fn no_miner(model: &str) -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, "no_miner_available", format!("no miner is serving {model:?}"))
            .retry_after(5)
    }

    pub fn overloaded(message: impl Into<String>) -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, "pool_overloaded", message).retry_after(2)
    }

    pub fn miner_failed(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_GATEWAY, "miner_failed", message)
    }

    pub fn bad_request(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, code, message)
    }

    pub fn param(mut self, param: &str) -> Self {
        self.param = Some(param.into());
        self
    }

    pub fn retry_after(mut self, seconds: u64) -> Self {
        self.retry_after = Some(seconds);
        self
    }

    pub fn with_request_id(mut self, request_id: &str) -> Self {
        self.request_id = Some(request_id.into());
        self
    }

    pub fn retryable(&self) -> bool {
        self.status == StatusCode::SERVICE_UNAVAILABLE
            || self.status == StatusCode::BAD_GATEWAY
            || self.status == StatusCode::GATEWAY_TIMEOUT
            || self.status == StatusCode::TOO_MANY_REQUESTS
    }

    pub fn body(&self) -> Value {
        let kind = if self.status.is_server_error() { "server_error" } else { "invalid_request_error" };
        json!({
            "error": {
                "message": self.message,
                "type": kind,
                "param": self.param,
                "code": self.code,
                "request_id": self.request_id,
                "retryable": self.retryable(),
            }
        })
    }
}

impl From<RequestError> for ApiError {
    fn from(error: RequestError) -> Self {
        Self { param: error.param, ..Self::bad_request(error.code, error.message) }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut response = (self.status, axum::Json(self.body())).into_response();
        if let Some(seconds) = self.retry_after {
            response.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from(seconds));
        }
        if let Some(id) = self.request_id.as_deref().and_then(|id| HeaderValue::from_str(id).ok()) {
            response.headers_mut().insert("x-request-id", id);
        }
        response
    }
}
