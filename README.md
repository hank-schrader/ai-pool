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
OpenAI-compatible `POST /v1/chat/completions`, with streaming.

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

A profile is one tested setting: these weights at this context need this much
accelerator memory. Miners only run published profiles. They never stretch
context into spare memory. Pin weight URLs to a repository revision, not `main`;
the sha256 is checked on every download. Restart the server after editing the
catalog.

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
