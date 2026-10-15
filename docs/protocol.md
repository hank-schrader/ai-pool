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

`GET /` serves the Svelte status page. `GET /v1/stats` and
`GET /v1/stats/stream` expose only pool totals, without miner/client identities.
These routes, `/assets/{path}`, `/healthz` and `/readyz` are unauthenticated,
including in `keys` mode; client, miner and admin routes keep their authentication.
The page uses same-origin assets and connections, with no inline scripts or styles.
Its HTML uses `Cache-Control: no-cache`, hashed assets use
`public, max-age=31536000, immutable`, and all embedded assets use
`X-Content-Type-Options: nosniff`. HTML CSP is exactly:

```text
default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self' data:; base-uri 'none'; frame-ancestors 'none'
```

`GET /v1/stats` sends `Cache-Control: public, max-age=5`, reading the latest
shared publication. Its existing top-level totals/history remain compatible;
`version`, `epoch`, `seq`, and `live` identify the publication. The workload
metrics `answered_last_hour`, `answering_questions` and `average_prompt_tokens`, and point/history
`answering`, `requests`, `prompt_tokens` are additive version 1 fields. Readers
should ignore unknown fields; older version 1 payloads may omit these metrics,
which the page displays as unavailable rather than zero. No per-question data
is published:

```json
{
  "version": 1,
  "epoch": "00000000-0000-4000-8000-000000000001",
  "seq": 123,
  "updated_at": "2026-10-14T12:02:05Z",
  "miners": 2,
  "queued_questions": 3,
  "answering_questions": 4,
  "average_prompt_tokens": 1500,
  "answered_per_second": 0.5,
  "answered_last_24h": 90,
  "answered_last_hour": 91,
  "live": [
    {"t": "2026-10-14T12:02:04Z", "answered": 1, "queued": 3, "miners": 2, "answering": 4, "requests": 1, "prompt_tokens": 1500}
  ],
  "history": {
    "interval_seconds": 60,
    "end": "2026-10-14T12:01:00Z",
    "answered": [60, 30],
    "queued": [0, 3],
    "miners": [2, 2],
    "answering": [4, 4],
    "requests": [20, 10],
    "prompt_tokens": [30000, 15000]
  }
}
```

`miners` counts connected, non-draining sessions, including sessions without
loaded models. `queued_questions` sums questions in jobs waiting for dispatch,
including retries waiting again; parsing, token counting and active jobs are
excluded. `answering_questions` sums the questions in dispatched/active jobs,
including attempts awaiting miner acknowledgement; it drops on completion,
cancellation, failure or requeue. Both totals are current values at publication,
not peaks. A systemone job contributes its `questions` entry count; a chat
completion contributes one. A successful job counts once regardless of retries,
only after the terminal success event reaches the client channel. Streams count
only when their final end event is enqueued.

`answered_per_second` is the rolling 60-second question count divided by 60.
`live` is oldest first, capped at 300 completed UTC seconds. The partial startup
second is excluded; its answers still contribute to the rolling rate. History
contains completed UTC minutes, oldest first, capped at 1,440 entries. The startup
minute is excluded, including its answers, even when startup aligns to a minute.
`answered` counts that bucket's successful answers; full-minute counts equal the
sum of their second buckets. Live `queued` is the **peak number of questions left waiting after a dispatch
pass during that second**; history `queued` is the **peak during that minute**.
Peaks are observed once per actor turn after dispatch, and at the next turn's
start. A job admitted and dispatched in the same turn never waited for a miner
and does not raise the peak. A job left waiting after dispatch raises the peak,
which persists for that bucket even if a later pass dispatches it within the same
second. Peaks reset independently at second/minute boundaries.
`miners` and `answering` sample closing boundary state (last known state during
catch-up). During catch-up, earlier bursts belong to their original buckets;
subsequent idle buckets carry the last known waiting count. `history.end` names the start of the newest
completed minute; arrays are empty until the first full minute closes.
`answered_last_24h` sums completed history buckets only.
`answered_last_hour` counts delivered questions in the current UTC minute so far
plus the 59 most recent completed UTC minutes: a rolling 60-minute-bucket window,
not an exact trailing 3,600 seconds. It includes the partial startup minute's
answers while that minute remains in the window, even though that minute is
excluded from history. Before 60 minute boundaries have elapsed since startup,
it covers all delivered answers since restart; the page estimates elapsed minutes
from history length and the startup minute and shows “since pool restart (N min)”.
The tile sparkline shows completed per-minute answered counts plus the current
minute's completed seconds, with the partial startup minute omitted as in history.

Bucket intervals are
half-open `[t, t + interval)` (1 second / 60 seconds). Timing uses monotonic
elapsed time anchored to UTC at startup. Catch-up work is bounded by retained
history capacity. All history is in memory and resets on restart.

`requests` counts successfully delivered requests in the bucket; a multi-question
request contributes one request and multiple `answered` questions. `prompt_tokens`
sums each delivered request's existing pre-submit `input_tokens` once. Full-minute
request/token sums equal the sums of their seconds. Retries, failures and terminal
events that cannot be enqueued in the client channel contribute nothing.
`average_prompt_tokens` is rolling 60-second `prompt_tokens / requests`, using
the same window as `answered_per_second`, and is **null** with no answered requests.
Zero-token requests yield a genuine zero average, distinct from an empty window.
Partial startup buckets still contribute to rolling totals but are excluded from
the corresponding completed histories.

**Prompt-size interpretation:** for multi-question h2o systemone, `input_tokens`
is the maximum token count among the separately rendered question prompts. Each
prompt includes the shared state, that question's instructions and options, and
the system/ChatML/assistant answer template. It is not the sum across questions,
and does not use the response's `usage.input_tokens` (which sums evaluated
question prompts). Thus the displayed average is the mean of the longest
single-question prompt per answered h2o request. Other backends use their existing
pre-submit exact input counter. The page labels the aggregate **Average prompt
size**, **per request, last 60 s**, and explains the h2o convention.
The main chart also shows prompt size on an independently scaled, zero-based
right axis in tokens. Live uses a request-weighted average over the last 10
completed seconds (`sum(prompt_tokens) / sum(requests)`); history uses each
minute's request-weighted average, including the provisional current minute.
Empty request windows are gaps, with isolated observations drawn as dots.
Both axes share tick positions, not units; orange prompt size and blue questions
per second must be read against their respective axes.

### Statistics SSE, version 1

`GET /v1/stats/stream` responds with `text/event-stream` and
`Cache-Control: no-store, no-transform`. Comment keep-alives occur every 15 seconds.
`POOL_MAX_STATS_STREAMS` is a positive integer, default **128**; connections at
capacity receive **503**, `Retry-After: 30`, and `Cache-Control: no-store` immediately.
The permit belongs to the response body and is released on disconnect. Readers
use bounded watch channels; there are no per-client unbounded queues or scheduler
commands. One process-wide publisher samples scheduler stats once per second,
skips missed timer ticks, and serializes each payload once. Publications occur
only when the completed-second endpoint advances.

Event names are `snapshot` and `tick`, each containing one JSON object. Shared
fields are `version` (integer, 1), `epoch` (UUID string identifying the server
process), `seq` (non-negative integer, incremented once per shared publication),
`updated_at` (RFC3339 UTC), and `totals` containing non-negative integer
`miners`, `queued_questions`, `answering_questions`, `answered_last_24h`, `answered_last_hour`, finite,
non-negative `answered_per_second`, and `average_prompt_tokens` (finite,
non-negative number or null). `snapshot` adds `live` and `history` with the shapes above.
`tick` adds `second` (one completed-second point) and `minute` (one completed-minute
point or null). Both points have `t` (RFC3339 UTC bucket start) and non-negative
integer `answered`, `queued`, `miners`, `answering`, `requests`, `prompt_tokens`.
History has parallel arrays for all six metrics.

```text
event: snapshot
data: {"version":1,"epoch":"00000000-0000-4000-8000-000000000001","seq":0,"updated_at":"2026-10-14T12:02:05Z","totals":{"miners":2,"queued_questions":3,"answered_last_24h":90,"answered_last_hour":91,"answered_per_second":0.5,"answering_questions":4,"average_prompt_tokens":1500},"live":[{"t":"2026-10-14T12:02:04Z","answered":1,"queued":3,"miners":2,"answering":4,"requests":1,"prompt_tokens":1500}],"history":{"interval_seconds":60,"end":"2026-10-14T12:01:00Z","answered":[60,30],"queued":[0,3],"miners":[2,2],"answering":[4,4],"requests":[20,10],"prompt_tokens":[30000,15000]}}

event: tick
data: {"version":1,"epoch":"00000000-0000-4000-8000-000000000001","seq":1,"updated_at":"2026-10-14T12:02:06Z","totals":{"miners":2,"queued_questions":3,"answered_last_24h":90,"answered_last_hour":91,"answered_per_second":0.5,"answering_questions":4,"average_prompt_tokens":1500},"second":{"t":"2026-10-14T12:02:05Z","answered":0,"queued":3,"miners":2,"answering":4,"requests":0,"prompt_tokens":0},"minute":null}

```

No SSE `id` is sent. `Last-Event-ID` is ignored. Every connection starts with a
snapshot whose `seq` identifies its shared publication. Adjacent ticks increment
`seq` by exactly one. Ignore duplicates; a sequence gap or epoch change requires
a replacement snapshot. Each shared frame retains its matching full snapshot.
When sampling skips completed seconds, the publisher emits a snapshot-only frame;
when a connection misses publications, it sends the latest snapshot.

The page defaults to Live: a muted per-second area and a blue trailing 10-second
average with a legend. Historical ranges (1 hour, 6 hours, 24 hours) append a
provisional current-minute average: questions so far divided by completed seconds
observed. Tooltip/table label it **so far**, mark incomplete startup coverage,
and replace it with the authoritative completed minute. At rollover the
provisional point is absent until one current-minute second has completed.
The hero remains the **60-second average**. Four tiles show connected miners,
questions being answered, questions waiting for a miner (current value, with a
per-minute peak sparkline), and average prompt size. Prompt sparklines use
per-minute request-weighted averages and have gaps where no requests completed.
Counts/token values use thousands separators. Tiles fit four across on wide
screens, two across on medium screens, and one column on mobile. The table adds
miners, answering questions, waiting peaks, answered requests, prompt-token sums,
and prompt tokens per request, including provisional current-minute metrics. On stream errors or gaps, the page
closes the stream, immediately polls `/v1/stats`, then polls every 10 seconds,
retaining data with an Offline icon and label. SSE retry uses exponential,
jittered backoff capped at 60 seconds, resetting after 30 seconds of healthy
ticks. The initial snapshot has a 10-second deadline. After receiving data, a
5-second watchdog counts only valid, advancing publications; duplicates and
keep-alive comments cannot refresh it. A timeout closes SSE, resumes polling and
backoff, and shows a neutral Reconnecting… indicator until advancing stream data
returns. Before any data or failure, the page shows neutral Connecting….
Hidden tabs abort polling and close SSE; becoming visible reconnects. Stream
deadlines and watchdogs are cleared on hide, connection replacement and cleanup.

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
