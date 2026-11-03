//! Client-facing HTTP API.

pub mod chat_tokens;
mod request;

use std::{collections::HashMap, convert::Infallible};

use axum::{
    Json,
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header as headers},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
};
use futures_util::stream;
use pool_protocol::{Model, Operation};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::{
    AppState,
    error::ApiError,
    scheduler::{JobEvent, JobGuard, ModelAvailability, NewJob},
};

/// Upper bound for reading a request body.
pub const MAX_BODY_BYTES: usize = 16 << 20;

pub async fn healthz() -> &'static str {
    "ok"
}

pub async fn stats(State(state): State<AppState>) -> Response {
    let frame = state.stats_publisher.latest();
    (
        [(headers::CACHE_CONTROL, "public, max-age=5"), (headers::CONTENT_TYPE, "application/json")],
        Body::from(frame.rest_json.to_string()),
    )
        .into_response()
}

pub async fn stats_stream(State(state): State<AppState>) -> Response {
    state.stats_publisher.response()
}

pub async fn readyz(State(state): State<AppState>) -> Json<Value> {
    Json(json!({
        "status": "ready",
        "catalog_revision": &*state.catalog_revision,
        "models": state.catalog.models.len(),
    }))
}

pub async fn list_models(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let snapshot = state.scheduler.snapshot().await;
    let data: Vec<Value> = state
        .catalog
        .models
        .iter()
        .map(|model| model_entry(&state.catalog, model, snapshot.models.get(&model.id).cloned().unwrap_or_default()))
        .collect();
    Ok(Json(json!({ "object": "list", "data": data })))
}

pub async fn get_model(State(state): State<AppState>, Path(id): Path<String>) -> Result<Json<Value>, ApiError> {
    let model = state.catalog.model(&id).ok_or_else(|| ApiError::model_not_found(&id))?;
    let snapshot = state.scheduler.snapshot().await;
    Ok(Json(model_entry(&state.catalog, model, snapshot.models.get(&id).cloned().unwrap_or_default())))
}

fn model_entry(catalog: &pool_protocol::Catalog, model: &Model, availability: ModelAvailability) -> Value {
    json!({
        "id": model.id,
        "object": "model",
        // no release date is recorded in the catalog
        "created": 0,
        "owned_by": "ai-pool",
        "pool": {
            "description": model.description,
            "model_revision": catalog.model_revision(model),
            "artifact": model.artifact,
            "runtime": catalog.runtime(&model.runtime),
            "execution": model.execution,
            "profiles": model.profiles,
            "capability_contract": if model.execution.chat_json { Some(pool_protocol::runtime::CHAT_JSON_CONTRACT) } else { None },
            "available_runtimes": availability.available_runtimes,
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

/// Miners reject unknown catalog fields, so offload profiles are only sent to
/// miners that ask for them with `?features=offload`, and h2o models only with
/// `?features=offload,h2o`. CPU profiles additionally require `cpu`.
pub async fn miner_catalog(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    state.auth.miner(&headers)?;
    let wants = |feature: &str| query.get("features").is_some_and(|f| f.split(',').any(|f| f == feature));
    if wants("chat-json-v1") {
        let mut catalog = (*state.catalog).clone();
        catalog.models.retain(|model| wants("h2o") || model.backend != pool_protocol::Backend::LlamaServerH2o);
        for model in &mut catalog.models {
            model.profiles.retain(|profile| {
                (wants("cpu") || profile.accelerator != pool_protocol::Accelerator::Cpu)
                    && (wants("offload") || !profile.offloaded())
            });
        }
        catalog.models.retain(|model| !model.profiles.is_empty());
        catalog.default_models.retain(|id| catalog.models.iter().any(|model| &model.id == id));
        return Ok(Json(json!({ "catalog_revision": catalog.revision(), "catalog": catalog })));
    }
    let (catalog, revision) = match (wants("offload"), wants("h2o"), wants("cpu")) {
        (_, true, true) => (&state.cpu_catalog, &state.cpu_catalog_revision),
        (_, true, false) => (&state.h2o_catalog, &state.h2o_catalog_revision),
        (true, false, _) => (&state.offload_catalog, &state.offload_catalog_revision),
        (false, false, _) => (&state.legacy_catalog, &state.legacy_catalog_revision),
    };
    if catalog.models.is_empty() {
        return Err(ApiError::new(
            StatusCode::UPGRADE_REQUIRED,
            "miner_outdated",
            "this pool serves only models that need a newer pool-miner; install the latest release",
        ));
    }
    Ok(Json(json!({ "catalog_revision": &**revision, "catalog": **catalog })))
}

pub async fn systemone(State(state): State<AppState>, body: Body) -> Response {
    infer(state, body, Operation::Systemone).await.unwrap_or_else(IntoResponse::into_response)
}

pub async fn chat_completions(State(state): State<AppState>, body: Body) -> Response {
    infer(state, body, Operation::ChatCompletions).await.unwrap_or_else(IntoResponse::into_response)
}

async fn infer(state: AppState, body: Body, operation: Operation) -> Result<Response, ApiError> {
    let prepared = request::prepare(&state, body, operation).await?;
    let counted = request::count(&state, &prepared).await?;
    let request::Prepared { request_id, model_id, model_revision, request, max_context, reservation } = prepared;
    let tag = |error: ApiError| error.with_request_id(&request_id);
    let input_tokens = counted.input_tokens;
    let required_context = request.required_context(input_tokens);
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
        client: "public".into(),
        model: model_id,
        model_revision: model_revision.clone(),
        request,
        input_tokens,
        required_context,
    };
    let buffer = if stream { state.config.stream_buffer_events } else { 1 };
    let mut guard = JobGuard::new(state.scheduler.clone(), &request_id, reservation);
    let mut events = state.scheduler.submit(job, buffer).await.map_err(tag)?;

    let mut headers = HeaderMap::new();
    headers.insert("x-request-id", header(&request_id));
    headers.insert("x-model-revision", header(&model_revision));

    if !stream {
        return match events.recv().await {
            Some(JobEvent::Result { body, profile, runtime, attempts }) => {
                guard.finish();
                headers.insert("x-model-profile", header(&profile));
                runtime_headers(&mut headers, runtime.as_deref());
                headers.insert("x-completion-attempts", HeaderValue::from(attempts));
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

fn runtime_headers(headers: &mut HeaderMap, runtime: Option<&pool_protocol::runtime::RuntimeDescriptor>) {
    if let Some(runtime) = runtime {
        headers.insert("x-runtime-fingerprint", header(&runtime.fingerprint));
        headers.insert("x-runtime-instance-id", header(&runtime.instance_id));
    }
}
