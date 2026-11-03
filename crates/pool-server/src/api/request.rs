use super::*;
use crate::scheduler::{CountReply, CountRequest, Reservation};
use pool_protocol::requests::Validated;
use serde_json::Map;

pub(super) struct Prepared {
    pub request_id: String,
    pub model_id: String,
    pub model_revision: String,
    pub request: Validated,
    pub max_context: u32,
    pub reservation: Reservation,
}

pub(super) async fn prepare(state: &AppState, body: Body, operation: Operation) -> Result<Prepared, ApiError> {
    let request_id = format!("req_{}", uuid::Uuid::new_v4().simple());
    let tag = |error: ApiError| error.with_request_id(&request_id);

    // Every inference request shares admission limits and an operational label.
    let client = "public".to_owned();
    // Refuse before reading, parsing or counting anything; the reservation is
    // released when this function returns or is dropped, or moves into the job guard.
    let reservation = state.scheduler.reserve(&client).map_err(tag)?;
    // per-model limits are checked after parsing; this bounds the read itself
    let body = axum::body::to_bytes(body, MAX_BODY_BYTES).await.map_err(|error| {
        tag(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "payload_too_large",
            format!("the request body could not be read (at most {MAX_BODY_BYTES} bytes): {error}"),
        ))
    })?;
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
    let request = pool_protocol::requests::validate(operation, model, fields).map_err(|error| tag(error.into()))?;
    let model_revision = state.catalog.model_revision(model);
    let max_context = model.max_context_tokens();
    Ok(Prepared { request_id, model_id, model_revision, request, max_context, reservation })
}

pub(super) async fn count(state: &AppState, prepared: &Prepared) -> Result<CountReply, ApiError> {
    state
        .scheduler
        .count(CountRequest {
            model: prepared.model_id.clone(),
            model_revision: prepared.model_revision.clone(),
            operation: prepared.request.operation,
            payload: prepared.request.count_payload.clone(),
            runtime_fingerprint: prepared.request.routing.runtime_fingerprint.clone(),
        })
        .await
        .map_err(|error| error.with_request_id(&prepared.request_id))
}
