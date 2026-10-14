# Miner ↔ pool protocol (`ai-pool.v1`)

Miners connect outbound, so they work behind NAT without any open ports. All
messages are JSON text frames, tagged by `type`; the types live in
`crates/pool-protocol/src/messages.rs`.

## Connection

1. `GET /miner/v1/catalog` returns `{"catalog_revision", "catalog"}`. The miner uses it to choose, download and start models. Miners reject unknown catalog fields, so the pool leaves out what a miner did not ask for: offload profiles unless `?features=offload`, and `h2o` models (backend `llama-server-h2o`, profile field `parallel_slots`) unless `?features=offload,h2o`.
2. `GET /miner/v1/connect` upgrades to WebSocket with subprotocol `ai-pool.v1`. In `keys` mode it needs `Authorization: Bearer <miner token>`; client API keys are rejected. Frames are limited to 4 MiB.
3. The miner sends `hello` within 10 s: protocol version, installation `miner_id`, version, catalog revision and hardware. The pool answers `welcome {session_id, heartbeat_seconds}`, or `error` and closes (e.g. `protocol_mismatch`).
4. The miner sends `capacity`, a full snapshot of devices (`slots` concurrent inferences each) and loaded models (`model`, `model_revision`, `profile`, `device`, `context_tokens`). The pool ignores entries whose revision or profile does not match its catalog. A new snapshot replaces the old one.
5. The miner sends `heartbeat {active_attempts}` every `heartbeat_seconds`. After three intervals of silence the pool drops the miner. The pool pings every 15 s.

On reconnect, a miner starts a fresh session. Attempts from the old session are never resumed, and their late results are discarded.

## Token counting

Before a request is queued, the pool asks one miner serving the model to count its input exactly:

- pool → `count_tokens {count_id, model, model_revision, operation, payload}`
- miner → `count_result {count_id, input_tokens}` or `count_error {count_id, code, message}`

Clef counts through `clef-token-count`, which runs the pinned llama-server's own prompt path. Chat models count through the runtime's `/v1/chat/completions/input_tokens`. H2O renders each question's prompt like refactor-tool and tokenizes it with its own llama-server (`/tokenize`); its count is the longest single question prompt, since questions run one after another. Counting uses no device slot.

The required context is the input tokens for systemone. For H2O a profile's `context_tokens` is an inclusive prompt limit, so a prompt of exactly that many tokens fits. A longer prompt is `context_length_exceeded`; a malformed question is `invalid_request`. For chat it is input + reserved output (`max_tokens`, default 512) + 1. The pool routes to the **smallest loaded profile that fits**, then the least busy device, then round robin.

Clients may set the optional top-level `min_context_tokens` on either
`POST /v1/systemone` or `POST /v1/chat/completions` (including streaming chat).
It must be a positive JSON integer no greater than the model's largest published
context. Invalid values, including `null`, produce HTTP 400 `invalid_request`
with `param: "min_context_tokens"`. The pool removes this field before counting
or forwarding the payload. It is a capacity requirement, not additional input
tokens or permission to exceed the model's context limit.

Placement and retries require a loaded profile of at least
`max(counted required context, min_context_tokens)`. The miner's job message still
carries the actual counted `required_context_tokens`; no protocol change is needed.
For example, a short Clef request with `"min_context_tokens": 8192` can run on
`cuda-8192` or `cuda-16384`, but cannot run on `cuda-4096`.

When only smaller miners currently serve the model, every request waits in the
ordinary queue for sufficient capacity to free up or
connect. `POOL_QUEUE_TIMEOUT_SECONDS` bounds that wait; expiry produces HTTP 503
`queue_timeout` with the usual Retry-After. A waiting request does not block
smaller requests behind it. This behavior applies with or without
`min_context_tokens`. If nobody serves the model, requests fail fast with
`no_miner_available`. A counted requirement beyond the model's largest published
context is rejected before queueing with HTTP 400 `context_length_exceeded`.

These requests consume the same queue limits and per-client admission reservations
as other requests. Automatic queue capacities still use connected miner slots
(including smaller profiles), not only slots eligible for a particular minimum.
Full queues return `pool_overloaded`; per-client limits return `rate_limit_exceeded`.

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

## Public status page and totals

`GET /` serves the pool's live status page, and `GET /v1/stats` returns its JSON
data. Both are public, including in `keys` mode; existing client, miner and admin
routes keep their authentication requirements. The page uses no external
resources, refreshes every 10 seconds, and retains its last data with an Offline
indicator if a refresh fails. Its content security policy permits its inline
script and styles and same-origin fetches; other resources are denied by default.

`GET /v1/stats` sends `Cache-Control: public, max-age=5`. Its response contains
only pool totals, with no miner or client identities:

```json
{
  "updated_at": "2026-10-14T12:02:05Z",
  "miners": 2,
  "queued_questions": 3,
  "answered_per_second": 0.5,
  "answered_last_24h": 90,
  "history": {
    "interval_seconds": 60,
    "end": "2026-10-14T12:01:00Z",
    "answered": [60, 30],
    "queued": [0, 3],
    "miners": [2, 2]
  }
}
```

`miners` counts connected, non-draining sessions, even if they have not announced
loaded models. `queued_questions` sums questions in jobs waiting for dispatch,
including retries waiting again; parsing, token counting and active jobs are
excluded. A systemone job contributes the number of entries in its `questions`
object; a chat completion contributes one. Only successful job completions count,
once per job regardless of retries. Streams count on successful completion.

`answered_per_second` is questions answered in the rolling 60-second window
divided by 60, using second buckets. History has one entry per completed UTC
minute since startup, oldest first, capped at 1,440 entries. The first bucket can
be a partial minute. `answered` counts successful answers during that minute;
`queued` and `miners` sample the scheduler at its closing boundary. `end` is the
start of the newest completed minute. Before the first boundary, arrays are empty
and `end` names the preceding UTC minute. `answered_last_24h` sums only completed
history buckets, so the current minute appears after it closes. All history is
in memory and resets when the pool restarts. Bucket timing uses monotonic elapsed
time anchored to UTC at startup.

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
| 503 | `pool_overloaded`, `queue_timeout`, `count_timeout` | capacity is temporarily exhausted |
| 504 | `inference_timeout` | the request deadline passed |
