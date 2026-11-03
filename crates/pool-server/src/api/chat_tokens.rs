use super::*;

/// Formats and tokenizes on a loaded miner; never invokes generation or stats.
pub async fn input_tokens(State(state): State<AppState>, body: Body) -> Response {
    count(state, body).await.unwrap_or_else(IntoResponse::into_response)
}

async fn count(state: AppState, body: Body) -> Result<Response, ApiError> {
    let prepared = request::prepare(&state, body, Operation::ChatCompletions).await?;
    let counted = request::count(&state, &prepared).await?;
    let loaded = counted.loaded;
    let runtime = loaded.runtime.as_deref();
    let mut headers = HeaderMap::new();
    headers.insert("x-request-id", header(&prepared.request_id));
    headers.insert("x-model-revision", header(&loaded.model_revision));
    headers.insert("x-model-profile", header(&loaded.profile));
    headers.insert("x-completion-attempts", HeaderValue::from_static("0"));
    runtime_headers(&mut headers, runtime);
    Ok((
        headers,
        Json(json!({
            "object": "response.input_tokens", "input_tokens": counted.input_tokens,
            "model": loaded.model, "model_revision": loaded.model_revision, "profile": loaded.profile,
            "context_tokens": loaded.context_tokens,
            "runtime_fingerprint": runtime.map(|r| &r.fingerprint),
            "runtime_instance_id": runtime.map(|r| &r.instance_id),
        })),
    )
        .into_response())
}
