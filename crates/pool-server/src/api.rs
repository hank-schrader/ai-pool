//! Client-facing HTTP API.

use std::convert::Infallible;

use axum::{
    Json,
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
};
use futures_util::stream;
use pool_protocol::{Model, Operation, requests};
use serde_json::{Map, Value, json};
use tokio::sync::mpsc;

use crate::{
    AppState,
    error::ApiError,
    scheduler::{CountRequest, JobEvent, JobGuard, ModelAvailability, NewJob},
};

pub async fn healthz() -> &'static str {
    "ok"
}

pub async fn readyz(State(state): State<AppState>) -> Json<Value> {
    Json(json!({
        "status": "ready",
        "catalog_revision": &*state.catalog_revision,
        "models": state.catalog.models.len(),
    }))
}

pub async fn list_models(State(state): State<AppState>, headers: HeaderMap) -> Result<Json<Value>, ApiError> {
    state.auth.client(&headers)?;
    let snapshot = state.scheduler.snapshot().await;
    let data: Vec<Value> = state
        .catalog
        .models
        .iter()
        .map(|model| model_entry(model, snapshot.models.get(&model.id).cloned().unwrap_or_default()))
        .collect();
    Ok(Json(json!({ "object": "list", "data": data })))
}

pub async fn get_model(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    state.auth.client(&headers)?;
    let model = state.catalog.model(&id).ok_or_else(|| ApiError::model_not_found(&id))?;
    let snapshot = state.scheduler.snapshot().await;
    Ok(Json(model_entry(model, snapshot.models.get(&id).cloned().unwrap_or_default())))
}

fn model_entry(model: &Model, availability: ModelAvailability) -> Value {
    json!({
        "id": model.id,
        "object": "model",
        // no release date is recorded in the catalog
        "created": 0,
        "owned_by": "ai-pool",
        "pool": {
            "description": model.description,
            "capabilities": [model.capability()],
            "streaming": model.backend.streaming(),
            "catalog_max_context_tokens": model.max_context_tokens(),
            "max_available_context_tokens": availability.max_available_context_tokens,
            "max_idle_context_tokens": availability.max_idle_context_tokens,
            "ready_miners": availability.ready_miners,
            "free_slots": availability.free_slots,
            "queued_requests": availability.queued_requests,
        }
    })
}

pub async fn admin_status(State(state): State<AppState>, headers: HeaderMap) -> Result<Json<Value>, ApiError> {
    state.auth.admin(&headers)?;
    let snapshot = state.scheduler.snapshot().await;
    Ok(Json(json!({ "catalog_revision": &*state.catalog_revision, "scheduler": snapshot })))
}

pub async fn miner_catalog(State(state): State<AppState>, headers: HeaderMap) -> Result<Json<Value>, ApiError> {
    state.auth.miner(&headers)?;
    Ok(Json(json!({ "catalog_revision": &*state.catalog_revision, "catalog": *state.catalog })))
}

pub async fn systemone(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    infer(state, headers, body, Operation::Systemone).await.unwrap_or_else(IntoResponse::into_response)
}

pub async fn chat_completions(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    infer(state, headers, body, Operation::ChatCompletions).await.unwrap_or_else(IntoResponse::into_response)
}

async fn infer(state: AppState, headers: HeaderMap, body: Bytes, operation: Operation) -> Result<Response, ApiError> {
    let request_id = format!("req_{}", uuid::Uuid::new_v4().simple());
    let tag = |error: ApiError| error.with_request_id(&request_id);

    let client = state.auth.client(&headers).map_err(tag)?;
    let fields: Map<String, Value> = serde_json::from_slice(&body).map_err(|error| {
        tag(ApiError::bad_request("invalid_request", format!("body must be a JSON object: {error}")))
    })?;
    let model_id = match fields.get("model") {
        Some(Value::String(id)) => id.clone(),
        _ => return Err(tag(ApiError::bad_request("invalid_request", "\"model\" is required").param("model"))),
    };
    let model = state.catalog.model(&model_id).ok_or_else(|| tag(ApiError::model_not_found(&model_id)))?;
    if body.len() > model.limits.request_bytes {
        return Err(tag(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "payload_too_large",
            format!("requests to {model_id:?} are limited to {} bytes", model.limits.request_bytes),
        )));
    }
    let request = requests::validate(operation, model, fields).map_err(|error| tag(error.into()))?;
    let model_revision = state.catalog.model_revision(model);

    let input_tokens = state
        .scheduler
        .count(CountRequest {
            model: model_id.clone(),
            model_revision: model_revision.clone(),
            operation,
            payload: request.count_payload.clone(),
        })
        .await
        .map_err(tag)?;
    let required_context = request.required_context(input_tokens);
    let max_context = model.max_context_tokens();
    if required_context > max_context {
        let reserved = match operation {
            Operation::Systemone => String::new(),
            Operation::ChatCompletions => format!(" ({input_tokens} input + {} output)", request.output_tokens),
        };
        return Err(tag(ApiError::bad_request(
            "context_length_exceeded",
            format!(
                "this request needs {required_context} tokens{reserved}; {model_id:?} supports at most {max_context}"
            ),
        )));
    }

    let stream = request.stream;
    let job = NewJob {
        id: request_id.clone(),
        client,
        model: model_id,
        model_revision: model_revision.clone(),
        request,
        input_tokens,
        required_context,
    };
    let buffer = if stream { state.config.stream_buffer_events } else { 1 };
    let mut events = state.scheduler.submit(job, buffer).await.map_err(tag)?;
    let mut guard = JobGuard::new(state.scheduler.clone(), &request_id);

    let mut headers = HeaderMap::new();
    headers.insert("x-request-id", header(&request_id));
    headers.insert("x-model-revision", header(&model_revision));

    if !stream {
        return match events.recv().await {
            Some(JobEvent::Result { body, profile }) => {
                guard.finish();
                headers.insert("x-model-profile", header(&profile));
                Ok((headers, Json(body)).into_response())
            }
            Some(JobEvent::Failed(error)) => {
                guard.finish();
                Err(error)
            }
            _ => Err(tag(ApiError::miner_failed("the job ended without a result"))),
        };
    }

    // Hold the HTTP response until the first chunk, so earlier failures
    // (and the one retry they allow) still produce a normal JSON error.
    let (first, ended) = match events.recv().await {
        Some(JobEvent::Chunk(chunk)) => (Event::default().data(chunk.to_string()), false),
        Some(JobEvent::End) => {
            guard.finish();
            (Event::default().data("[DONE]"), true)
        }
        Some(JobEvent::Failed(error)) => {
            guard.finish();
            return Err(error);
        }
        _ => return Err(tag(ApiError::miner_failed("the stream ended before it started"))),
    };
    let relay = Relay { events, guard, first: Some(first), done_after_first: ended, done: false };
    let body = Sse::new(stream::unfold(relay, Relay::next)).keep_alive(KeepAlive::default());
    Ok((headers, body).into_response())
}

/// Relays scheduler events as SSE. Dropping it (client disconnect) cancels the job.
struct Relay {
    events: mpsc::Receiver<JobEvent>,
    guard: JobGuard,
    first: Option<Event>,
    done_after_first: bool,
    done: bool,
}

impl Relay {
    async fn next(mut self) -> Option<(Result<Event, Infallible>, Self)> {
        if self.done {
            return None;
        }
        if let Some(first) = self.first.take() {
            self.done = self.done_after_first;
            return Some((Ok(first), self));
        }
        let event = match self.events.recv().await {
            Some(JobEvent::Chunk(chunk)) => return Some((Ok(Event::default().data(chunk.to_string())), self)),
            Some(JobEvent::End) => Event::default().data("[DONE]"),
            Some(JobEvent::Failed(error)) => Event::default().data(error.body().to_string()),
            Some(JobEvent::Result { .. }) | None => {
                Event::default().data(ApiError::miner_failed("the stream was interrupted").body().to_string())
            }
        };
        // after an error the stream closes without [DONE]
        self.guard.finish();
        self.done = true;
        Some((Ok(event), self))
    }
}

fn header(value: &str) -> HeaderValue {
    HeaderValue::from_str(value).unwrap_or_else(|_| HeaderValue::from_static("invalid"))
}
