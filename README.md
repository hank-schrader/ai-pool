# ai-pool

A compute pool for AI models. Clients send inference requests to the **pool
server**. The server routes each one to a **miner**, a machine with a GPU that
has downloaded the model, and relays the answer back.

```
client ──HTTP──▶ pool-server ◀──WebSocket── pool-miner ──▶ llama-server (local GPU)
```

- **pool-server** (Rust, axum) serves an OpenAI-shaped API, keeps the model catalog, and queues and schedules jobs. It needs no GPU.
- **pool-miner** (Rust CLI for Linux, Windows and macOS) connects out to a pool, so no open ports are needed. You pick which catalog models to serve. It downloads verified weights and a pinned llama.cpp runtime, then serves jobs.
- **Model catalog** (`config/models.json`): for each model, its API name, description, weight download and sha256, and context profiles with their measured VRAM.

The default model is **Clef** (`clef`), a decision model that answers choice,
score and yes/no questions with probabilities through `POST /v1/systemone`.
The catalog also ships a small chat model, `qwen2.5-1.5b-instruct`, for
OpenAI-compatible `POST /v1/chat/completions`, with streaming, and **H2O**
(`h2o`), the BF16 H2O-Lightning-4B decision model of
[refactor-tool](https://github.com/hank-schrader/refactor-tool), also on
`POST /v1/systemone`. H2O reads each question on its own and answers like
refactor-tool's local engine with its default policy `single`: same prompt,
label-only forward pass, temperature 0.8 and yes/no commit rule. Miners run
llama-server with 2048-token prompt chunks and no RAM prompt cache, which
answered 5% more questions (`docs/qualification/h2o-flags.md`), so
`answers.<id>.noul` and `answers.<id>.probabilities` can differ from the local
numbers in the last digits.
Questions take 1-16 options; `usage.input_tokens` sums the prompts of all
questions.

## Quickstart (one machine)

You need Rust, an NVIDIA GPU (Linux or Windows) or Apple Silicon, and CMake plus a C++ compiler to build the Clef token counter once.

```sh
# 1. Server, with defaults that work out of the box (see .env.example)
cargo run

# 2. In another terminal: build the Clef token counter (CPU-only, about 1 minute)
scripts/build-helper.sh

# 3. Miner: choose models interactively, or name them
cargo run -p pool-miner -- --pool http://127.0.0.1:8080 --models clef,qwen2.5-1.5b-instruct --yes
```

The miner checks the GPU and picks the largest context profile per model that
fits. It then downloads the pinned runtime and the weights (Clef is 9.66 GB),
verifies them, and connects to the pool. Then:

```sh
curl http://127.0.0.1:8080/v1/models

curl http://127.0.0.1:8080/v1/systemone -H 'Content-Type: application/json' -d '{
  "model": "clef",
  "state": "Customer message: I was charged twice for my order last week.",
  "questions": {
    "route":  {"type": "choice", "instructions": "Which team should handle this?",
               "criteria": {"billing": null, "shipping": null, "technical": null}},
    "angry":  {"type": "noul", "instructions": "Is the customer angry?"},
    "urgency": {"type": "score", "instructions": "How urgent is this?",
               "criteria": ["can wait", "this week", "today", "right now"]}
  }
}'

curl -N http://127.0.0.1:8080/v1/chat/completions -H 'Content-Type: application/json' -d '{
  "model": "qwen2.5-1.5b-instruct",
  "messages": [{"role": "user", "content": "Explain compute pools in two sentences."}],
  "max_tokens": 128, "stream": true, "stream_options": {"include_usage": true}
}'
```

## Miner commands

```sh
pool-miner list-models                      # catalog models and which profiles fit this GPU
pool-miner doctor                           # GPU, driver, runtime, helper and cache checks
pool-miner download clef --import <file>    # reuse an existing verified .gguf (hard link, no copy)
pool-miner --models clef --context clef=16384   # force a profile instead of the automatic plan
pool-miner cache list | verify <model> | remove <model>
```

With several models, the miner first reserves each model's smallest profile,
then raises them in the order given, as far as the GPU's free memory allows. On
a 16 GB RTX 5070 Ti that gives `clef` at 8192 and `qwen2.5-1.5b-instruct` at
16384 (13.3 GiB). The llama.cpp runtime is only marked qualified for Linux +
CUDA so far. On Windows and macOS, pass `--allow-unqualified-runtime` until
those platforms have been tested (`docs/qualification/`).

## Benchmarking a miner

```sh
pool-miner bench --models h2o,qwen2.5-1.5b-instruct --duration 60 --seed 20261012 --yes
# Offline catalog; imported or cached weights use the same verified cache:
pool-miner bench --catalog config/models.json --models h2o --json
```

Without `--models`, choose models interactively. Bench fetches only the catalog
from `--pool`, then runs each fitting profile separately without a pool session.
When a full-GPU profile fits, bench skips that model's published offload profiles.
Otherwise it tests fitting offload profiles, or uses automatic RAM fallback when
no published placement fits. `--profile MODEL=ID`
narrows the test to one profile; `--context MODEL=TOKENS` tests every fitting
published placement at that context. Downloads ask for confirmation unless
`--yes` is set. Import weights first with `pool-miner download MODEL --import FILE`.

The seeded workload uses the same sequence of synthetic records at every
profile: 40% target 300–1,500 tokens, 30% 1,500–4,000, 20% 4,000–12,000,
and 10% 12,000–40,000. Lengths approximate four bytes per token. Records carry
1–8 `noul`/`choice` questions per systemone request, or request short chat responses.
Exact token counting trims records to fit each profile, leaving room for
templates and chat output. Concurrency
matches the miner: H2O slots plus prefetch (`--prefetch`), one job for Clef/chat.
One concurrency wave warms up each profile before measurement. The default
60 seconds admits work, then outstanding requests drain with a 150-second
timeout per request. Rates use actual elapsed measurement time, including drain.

The final table shows:

- **Model / Profile / Place / Jobs**: abbreviated model, published profile ID,
  GPU/offload/automatic RAM fallback, and requests allowed in flight.
- **Req/s / Q/s**: successful requests and systemone questions per second.
- **In/s / Out/s**: prompt and chat generated tokens per second, from response
  usage; H2O prompt usage sums all question prompts, including reported prefixes.
- **p50 s / p95 s / Err**: successful request latency including runtime queueing,
  excluding preparation and warm-up, and failed/timed-out requests.
- **VRAM / RSS**: sampled runtime PID peaks in MiB, including warm-up and drain;
  unavailable readings show `—`. Samples are taken every 500 ms.

Fewer than ten completed requests produces a small-sample warning. Failed
profiles remain in the report and make the command exit unsuccessfully.
`--json` writes one report to stdout, including full names, settings, raw counts,
error breakdowns, warnings, runtime revisions, unavailable-reading reasons, and
the benchmark process's separate peak RSS. The human table and progress go to
stderr when using `--json`; stdout remains one JSON document.

## Serving other machines

Local mode (the default) has no credentials and only listens on loopback. To accept remote clients and miners:

```sh
# .env on the pool host
POOL_BIND=0.0.0.0:8080
POOL_AUTH_MODE=keys
POOL_CLIENT_API_KEYS=<client key>,<another client key>
POOL_MINER_TOKENS=<token for each approved miner>
POOL_ADMIN_TOKEN=<token for GET /admin/v1/status>
```

Then on each miner machine:

```sh
pool-miner --pool http://<pool-host>:8080 --token <miner token>
```

To let **anyone** run a miner without a token, set `POOL_MINER_AUTH=open` (clients still need API keys). Then a new machine only needs:

```sh
curl -fsSL https://raw.githubusercontent.com/hank-schrader/ai-pool/main/scripts/install.sh | sh   # then open a new terminal
pool-miner --pool https://<pool-host>
```

The miner lists the pool's models with what fits this GPU, asks which to serve, confirms the download, then installs the runtime, downloads and verifies the weights, checks each model, and connects. Anonymous miners see the requests they serve and could return wrong answers, so open pools suit data you are fine sharing. `POOL_MAX_MINERS` (default 256) caps how many can connect.

Clients send `Authorization: Bearer <client key>`. Put the pool behind a TLS reverse proxy (HTTPS/WSS) when it is reachable from outside your network. Miners see the requests they serve, so only approve miners you trust with that data.

## Installing a miner from a release

Release archives contain `pool-miner`, `pool-server`, `clef-token-count` and the default catalog, so no compiler or CUDA toolkit is needed. NVIDIA miners need a recent driver.

```sh
curl -fsSL https://raw.githubusercontent.com/hank-schrader/ai-pool/main/scripts/install.sh | sh   # Linux, macOS
irm https://raw.githubusercontent.com/hank-schrader/ai-pool/main/scripts/install.ps1 | iex      # Windows
```

The installers add the binaries to your PATH (`~/.local/bin`, written to your shell's startup file; on Windows, the user PATH), so open a new terminal afterwards. Pass `--no-modify-path` to `install.sh` to skip that. Releases are published by pushing a `v*` tag (see `.github/workflows/release.yml`).

## API

| Endpoint | |
|---|---|
| `GET /v1/models`, `GET /v1/models/{id}` | catalog plus live availability (`ready_miners`, `free_slots`, `max_available_context_tokens`, …) |
| `POST /v1/systemone` | Clef decisions; [System One request format](https://github.com/ggml-org/llama.cpp/blob/b92761a515ea31e852e7fbc1fad5f874b46f3718/tools/server/README.md) plus `model` |
| `POST /v1/chat/completions` | text chat subset of the OpenAI API, JSON or SSE; tools, images and `response_format` are rejected |
| `GET /admin/v1/status` | miners, devices, loaded profiles, queue |
| `GET /healthz`, `GET /readyz` | liveness and readiness |

Errors are OpenAI-shaped, with `code`, `request_id` and `retryable`; see [docs/protocol.md](docs/protocol.md).

Both inference endpoints accept an optional top-level `min_context_tokens`: a
positive integer up to the model's largest published context. It requires a
miner with that much context or more, even for a shorter prompt. For example:

```json
{
  "model": "clef",
  "min_context_tokens": 8192,
  "state": "Please refund the duplicate charge.",
  "questions": {"refund": {"type": "noul", "instructions": "Is a refund requested?"}}
}
```

The pool routes using the larger of this minimum and the actual required context
(chat includes reserved output). It strips the field before token counting and
inference. If only smaller miners serve the model, the request waits for a
matching miner to connect or free up, subject to normal queue limits and
`POOL_QUEUE_TIMEOUT_SECONDS` (503 `queue_timeout`), whether or not the minimum
is supplied. No miner serving the model remains a fast 503 `no_miner_available`;
invalid minimums return 400 `invalid_request` with `param: "min_context_tokens"`.
A counted requirement beyond the model's largest published context returns
400 `context_length_exceeded`.

## Model catalog

`config/models.json` is validated at startup (`cargo run -- check-config`). Each model defines:

```json
{
  "id": "clef",
  "description": "…",
  "backend": "llama-server-systemone",
  "runtime": "llama-b11374",
  "weights": {"filename": "…gguf", "url": "https://…", "size_bytes": 9657260096, "sha256": "…"},
  "limits": {"request_bytes": 1048576, "max_questions": 16, "max_choice_options": 64, "max_score_levels": 10},
  "profiles": [
    {"id": "cuda-16384", "accelerator": "cuda", "context_tokens": 16384, "memory_mib": 13312, "memory_source": "measured"}
  ]
}
```

Backends: `llama-server-systemone` (Clef), `llama-server-chat`, and
`llama-server-h2o`. An H2O profile's `context_tokens` is the longest accepted
prompt, per slot (the server gets two more tokens, as in refactor-tool), and the
optional `parallel_slots` is the llama-server slot count: `h2o`'s
`cuda-40960-x3` runs 3 slots of 40,960 tokens (+12,472 MiB peak on an RTX 5070 Ti,
see `docs/qualification/h2o-offload.md`), `cuda-8192` runs one. `cuda-40960-x4`
(16 GB cards) and `cuda-40960-x6` (24 GB cards) are memory estimates, not yet
measured. Slots share the weights and keep several records cached. An H2O miner
accepts as many jobs as slots plus a prefetch (default: as many again, `--prefetch N`
changes it, 0 turns it off), so the next questions are prepared while the GPU works;
llama-server queues the extra requests, and the job deadline includes that wait.
Clef and chat models run one job at a time.

A profile is one tested setting: these weights at this context need this much
accelerator memory. **Offload profiles** (`cuda-<context>-gpu<layers>`, with
`gpu_layers` and `host_memory_mib`) keep only some layers on an NVIDIA GPU and the
rest in system RAM. Clef runs on GPUs with as little as about 2 GB of VRAM when
there is about 11 GB of free RAM, at nearly full speed. The miner picks them only
when nothing fits fully on the GPU. If no published placement fits a selected
CUDA model, the miner can automatically keep all its weights in system RAM
(`-ngl 0`) at its smallest published context and slot count. It reserves estimated
VRAM for CUDA buffers and estimated RAM for weights and runtime overhead, then
requires the existing near-full-context probe to answer within 120 seconds before
serving. A failed probe leaves the model unavailable. The plan and model list
label this placement as an **automatic RAM fallback (estimate)**; it can be much
slower than GPU inference.

An explicit `--profile` or `--context` may also use RAM fallback while keeping
the requested setting. The miner reports the published profile ID to the pool.
Fallback does not resolve combined memory pressure when selected models fit
individually but cannot fit together. Miners only use published contexts and
slot counts. They never stretch context into spare memory.
Pin weight URLs to a repository revision, not `main`;
the sha256 is checked on every download. Restart the server after editing the
catalog.

## Queue and client limits

The pool sizes its queues from the capacity it has. `POOL_QUEUE_CAPACITY_GLOBAL` and
`POOL_QUEUE_CAPACITY_PER_MODEL`, when unset, are automatic: computed at admission time
from the connected, non-draining miners as 4 x their slots (per model: only miners
serving that model), at least 16. Setting a number makes it a fixed operator cap.
When a capacity is reached the server answers 503 `pool_overloaded` with
`Retry-After` (the global queue is checked before the request body is read); a model nobody serves gets `no_miner_available`.

`POOL_MAX_ACTIVE_PER_CLIENT` and `POOL_MAX_QUEUED_PER_CLIENT` are unset by default:
no per-client limit. When set (positive), a client may have that many running and
waiting requests, and the active cap also holds back its queued jobs. Dispatch is
strictly FIFO (oldest queued job first); without per-client limits one client that
floods the queue is served ahead of later arrivals, so set the per-client caps if
clients must be isolated from each other.

When both per-client caps are set, a client may have at most their sum in the whole
pipeline, including requests still being read, parsed or token-counted: the slot is
reserved right after authentication and a request over the limit gets 429
`rate_limit_exceeded` before its body is read. The slot is released when the response
ends, the request fails or the client disconnects. `Retry-After` for 429 and
`pool_overloaded` is `ceil(jobs ahead / completions per second)` over the last 60 s
(the client's own completions for 429, the pool's for 503), clamped to 1..60 s;
60 when nothing completed recently.

## Repository

| Path | |
|---|---|
| `crates/pool-protocol` | catalog types and validation, wire messages, request validation |
| `crates/pool-server` | HTTP API, auth, scheduler, miner connections |
| `crates/pool-runtime` | verified downloads, runtime install, llama-server processes, local clients |
| `crates/pool-miner` | the miner CLI |
| `native/clef-token-count` | exact Clef token counter built from pinned llama.cpp |
| `runtime/manifest.json` | pinned llama.cpp release archives and hashes per platform |
| `docs/PLAN.md` | architecture and decisions |
| `docs/qualification/` | measured runtime behaviour and memory per platform |
