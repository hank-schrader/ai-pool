//! Loopback HTTP client for a managed llama-server.

use std::time::Duration;

use futures_util::StreamExt;
use pool_protocol::messages::JobErrorCode;
use reqwest::StatusCode;
use serde_json::Value;

use crate::sse::{SseEvent, SseParser};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeError {
    pub code: JobErrorCode,
    pub message: String,
}

impl RuntimeError {
    pub fn new(code: JobErrorCode, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }

    fn failed(message: impl Into<String>) -> Self {
        Self::new(JobErrorCode::RuntimeFailed, message)
    }
}

impl std::fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.code, self.message)
    }
}

impl std::error::Error for RuntimeError {}

#[derive(Clone, Debug)]
pub struct LocalClient {
    http: reqwest::Client,
    base: String,
    api_key: String,
}

impl LocalClient {
    pub fn new(base: String, api_key: String) -> Self {
        let http = reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(Duration::from_secs(5))
            .build()
            .expect("loopback HTTP client builds");
        Self { http, base, api_key }
    }

    pub fn base_url(&self) -> &str {
        &self.base
    }

    pub async fn healthy(&self) -> bool {
        let request = self.http.get(format!("{}/health", self.base)).bearer_auth(&self.api_key);
        matches!(request.timeout(Duration::from_secs(2)).send().await, Ok(response) if response.status().is_success())
    }

    pub async fn systemone(&self, payload: &Value) -> Result<Value, RuntimeError> {
        self.post_json("/v1/systemone", payload).await
    }

    pub async fn chat(&self, payload: &Value) -> Result<Value, RuntimeError> {
        self.post_json("/v1/chat/completions", payload).await
    }

    pub async fn chat_input_tokens(&self, payload: &Value) -> Result<u32, RuntimeError> {
        let reply = self.post_json("/v1/chat/completions/input_tokens", payload).await?;
        reply
            .get("input_tokens")
            .and_then(Value::as_u64)
            .map(|n| n as u32)
            .ok_or_else(|| RuntimeError::new(JobErrorCode::Internal, "input_tokens missing from runtime reply"))
    }

    /// Starts a streaming chat request; the stream yields parsed upstream events.
    pub async fn chat_stream(&self, payload: &Value) -> Result<ChatStream, RuntimeError> {
        let response = self.send("/v1/chat/completions", payload).await?;
        let response = check_status(response).await?;
        Ok(ChatStream { body: Box::pin(response.bytes_stream()), parser: SseParser::default(), pending: Vec::new() })
    }

    async fn post_json(&self, path: &str, payload: &Value) -> Result<Value, RuntimeError> {
        let response = check_status(self.send(path, payload).await?).await?;
        response.json().await.map_err(|error| RuntimeError::failed(format!("invalid runtime response: {error}")))
    }

    async fn send(&self, path: &str, payload: &Value) -> Result<reqwest::Response, RuntimeError> {
        self.http
            .post(format!("{}{}", self.base, path))
            .bearer_auth(&self.api_key)
            .json(payload)
            .send()
            .await
            .map_err(|error| RuntimeError::failed(format!("runtime unreachable: {error}")))
    }
}

async fn check_status(response: reqwest::Response) -> Result<reqwest::Response, RuntimeError> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let text = response.text().await.unwrap_or_default();
    Err(map_error(status, &text))
}

/// Maps a runtime HTTP error onto a job error code.
pub fn map_error(status: StatusCode, body: &str) -> RuntimeError {
    let message = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| value.pointer("/error/message").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| body.chars().take(500).collect());
    classify(status, &message)
}

fn classify(status: StatusCode, message: &str) -> RuntimeError {
    let too_long =
        message.contains("is too large to process") || message.contains("exceeds the available context size");
    let code = if too_long {
        JobErrorCode::ContextLengthExceeded
    } else if status.is_client_error() {
        JobErrorCode::InvalidRequest
    } else {
        JobErrorCode::RuntimeFailed
    };
    RuntimeError::new(code, format!("runtime HTTP {}: {message}", status.as_u16()))
}

pub enum StreamItem {
    Chunk(Value),
    Done,
}

pub struct ChatStream {
    body: std::pin::Pin<Box<dyn futures_util::Stream<Item = reqwest::Result<bytes::Bytes>> + Send>>,
    parser: SseParser,
    pending: Vec<SseEvent>,
}

impl ChatStream {
    /// Next upstream event; `None` means the connection ended (without `[DONE]`
    /// if no `Done` was returned before).
    pub async fn next(&mut self) -> Option<Result<StreamItem, RuntimeError>> {
        loop {
            if !self.pending.is_empty() {
                let event = self.pending.remove(0);
                return Some(match event {
                    SseEvent::Done => Ok(StreamItem::Done),
                    SseEvent::Data(data) => match serde_json::from_str::<Value>(&data) {
                        Ok(chunk) if chunk.get("error").is_some() => {
                            let message =
                                chunk.pointer("/error/message").and_then(Value::as_str).unwrap_or("stream error");
                            Err(classify(StatusCode::INTERNAL_SERVER_ERROR, message))
                        }
                        Ok(chunk) => Ok(StreamItem::Chunk(chunk)),
                        Err(error) => Err(RuntimeError::failed(format!("invalid stream event: {error}"))),
                    },
                });
            }
            match self.body.next().await? {
                Ok(bytes) => match self.parser.push(&bytes) {
                    Ok(events) => self.pending.extend(events),
                    Err(error) => return Some(Err(RuntimeError::failed(error))),
                },
                Err(error) => return Some(Err(RuntimeError::failed(format!("stream interrupted: {error}")))),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_context_errors() {
        let clef = r#"{"error":{"code":500,"message":"input (4475 tokens) is too large to process. increase the physical batch size (current batch size: 4096)"}}"#;
        assert_eq!(map_error(StatusCode::INTERNAL_SERVER_ERROR, clef).code, JobErrorCode::ContextLengthExceeded);
        let chat = r#"{"error":{"code":400,"message":"request (4194 tokens) exceeds the available context size (4096 tokens), try increasing it"}}"#;
        assert_eq!(map_error(StatusCode::BAD_REQUEST, chat).code, JobErrorCode::ContextLengthExceeded);
        assert_eq!(map_error(StatusCode::BAD_REQUEST, "{}").code, JobErrorCode::InvalidRequest);
        assert_eq!(map_error(StatusCode::SERVICE_UNAVAILABLE, "loading").code, JobErrorCode::RuntimeFailed);
    }
}
