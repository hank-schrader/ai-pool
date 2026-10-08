# Miner ↔ pool protocol (`ai-pool.v1`)

Miners connect outbound, so they work behind NAT without any open ports. All
messages are JSON text frames, tagged by `type`; the types live in
`crates/pool-protocol/src/messages.rs`.

## Connection

1. `GET /miner/v1/catalog` returns `{"catalog_revision", "catalog"}`. The miner uses it to choose, download and start models.
2. `GET /miner/v1/connect` upgrades to WebSocket with subprotocol `ai-pool.v1`. In `keys` mode it needs `Authorization: Bearer <miner token>`; client API keys are rejected. Frames are limited to 4 MiB.
3. The miner sends `hello` within 10 s: protocol version, installation `miner_id`, version, catalog revision and hardware. The pool answers `welcome {session_id, heartbeat_seconds}`, or `error` and closes (e.g. `protocol_mismatch`).
4. The miner sends `capacity`, a full snapshot of devices (`slots` concurrent inferences each) and loaded models (`model`, `model_revision`, `profile`, `device`, `context_tokens`). The pool ignores entries whose revision or profile does not match its catalog. A new snapshot replaces the old one.
5. The miner sends `heartbeat {active_attempts}` every `heartbeat_seconds`. After three intervals of silence the pool drops the miner. The pool pings every 15 s.

On reconnect, a miner starts a fresh session. Attempts from the old session are never resumed, and their late results are discarded.

## Token counting

Before a request is queued, the pool asks one miner serving the model to count its input exactly:

- pool → `count_tokens {count_id, model, model_revision, operation, payload}`
- miner → `count_result {count_id, input_tokens}` or `count_error {count_id, code, message}`

Clef counts through `clef-token-count`, which runs the pinned llama-server's own prompt path. Chat models count through the runtime's `/v1/chat/completions/input_tokens`. Counting uses no device slot.

The required context is the input tokens for systemone. For chat it is input + reserved output (`max_tokens`, default 512) + 1. The pool routes to the **smallest loaded profile that fits**, then the least busy device, then round robin.

## Jobs

- pool → `job {job_id, attempt_id, model, model_revision, profile, operation, stream, input_tokens, required_context_tokens, timeout_ms, payload}`
- miner → `accepted {attempt_id}` within 5 s, or `rejected {attempt_id, reason}` (`busy`, `model_unavailable`, `revision_mismatch`, `context_too_small`, `draining`)
- non-streaming: miner → `result {attempt_id, body}` (the runtime's JSON response)
- streaming: miner → `stream_start`, then `stream_chunk {attempt_id, seq, chunk}` with `seq` counting from 0, one per upstream SSE event, then `stream_end {attempt_id, usage}`. The runtime's usage-only chunk is not forwarded; its usage comes in `stream_end`, and the pool emits the client's usage chunk only if `stream_options.include_usage` was set.
- failure: miner → `job_error {attempt_id, code, message}`, where code is `invalid_request`, `context_length_exceeded`, `runtime_failed`, `timeout` or `internal`.
- pool → `cancel {attempt_id}` when the client disconnects, the deadline passes, or a stream client falls more than `POOL_STREAM_BUFFER_EVENTS` behind. The miner aborts the local request and confirms with `cancelled {attempt_id}`. The device slot stays reserved until then.
- miner → `draining` means no new jobs; active ones finish.

### Retries and commitment

A job is retried **once**, on a different miner, if its miner disconnects, fails with `runtime_failed`, `timeout` or `internal`, or does not acknowledge in time, and only before anything reached the client. A stream is committed by its first chunk. After that, a failure ends the client's SSE stream with an `error` event and no `[DONE]`, and output is never replayed. Rejections are re-queued up to three times without using up the retry.

## Client-facing errors

OpenAI-shaped: `{"error": {"message", "type", "param", "code", "request_id", "retryable"}}`, plus an `x-request-id` header, and `retry-after` for temporary capacity errors.

| Status | `code` | When |
|---|---|---|
| 400 | `invalid_request`, `unsupported_parameter`, `unsupported_capability`, `unsupported_streaming` | request does not fit the model's contract |
| 400 | `context_length_exceeded` | needs more context than any catalog profile |
| 401 | `unauthorized` | missing or wrong credentials (keys mode) |
| 404 | `model_not_found` | model is not in the catalog |
| 413 | `payload_too_large` | body exceeds the model's `limits.request_bytes` |
| 429 | `rate_limit_exceeded` | client has too many running and queued requests |
| 502 | `miner_failed` | the miner failed and the retry is used up or was not allowed |
| 503 | `no_miner_available` | no miner serves the model |
| 503 | `context_unavailable` | the catalog allows the context, but no loaded profile does |
| 503 | `pool_overloaded`, `queue_timeout`, `count_timeout` | capacity is temporarily exhausted |
| 504 | `inference_timeout` | the request deadline passed |
