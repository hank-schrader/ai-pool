# Phase 0 qualification: Linux x86_64 + NVIDIA CUDA

Date: 2026-10-08. Script: `scripts/qualify/measure.py`. Raw results: `data/clef-cuda12.8.jsonl`, `data/chat-cuda12.8.jsonl`.

## Machine

| | |
|---|---|
| OS | Arch Linux, kernel 7.2.9 |
| CPU / RAM | AMD Ryzen 9 9950X, 30 GiB |
| GPU | NVIDIA GeForce RTX 5070 Ti, 16,303 MiB, driver 615.71.09 |
| Desktop baseline | 1,415 MiB VRAM in use before each run |

## Runtime artifacts (llama.cpp release `b11374` = commit `b92761a515ea31e852e7fbc1fad5f874b46f3718`)

| Asset | Bytes | SHA-256 (matches GitHub release digest) |
|---|---:|---|
| `llama-b11374-bin-ubuntu-cuda-12.8-x64.tar.gz` | 171,513,778 | `bd3885b4997a25afc22e9d568fd646d223fec97ca0ccd1a65ab1d423edf624f9` |
| `cudart-llama-b11374-bin-ubuntu-cuda-12.8-x64.tar.gz` | 594,377,951 | `19446ae5d77290502096a7f1b9d71472d9fe8e6e90789d767af3ccef15862d35` |

- Extract both into one directory. The cudart archive supplies `libcudart.so.12`, `libcublas.so.12` and `libcublasLt.so.12`. `llama-server` has `RUNPATH=$ORIGIN`, so it finds its own libraries without `LD_LIBRARY_PATH`.
- With an empty environment, `llama-server --version` reports `version: 0.5.0-dev (build 11374, commit b92761a51)`.
- Libraries still needed from the system: glibc, libstdc++, libgcc_s, libgomp, OpenSSL 3 (`libssl.so.3`, `libcrypto.so.3`), zlib, brotli and zstd. The libcuda driver library comes from the NVIDIA driver. `pool-miner doctor` must check for these.
- CUDA 12.8 was chosen over 13.4 because it supports older drivers (≥ 570) and still includes Blackwell (sm_120). The CUDA 13.4 build has not been tested.

## Clef-Flash Q8_0 (`8754b06f…`, HF revision `4a7a08c0…`)

Launch: `-ngl 99 -c N -b N -ub N --parallel 1`.

| Profile | Peak VRAM above baseline | Load | Small request (315 tok) | Near-limit request | Over-limit request |
|---|---:|---:|---|---|---|
| 4096 | **9,324 MiB** | 3.0 s | 200 | 3,973 tok, 0.73 s | 500 `input (4475 tokens) is too large to process…` |
| 8192 | **10,184 MiB** | 1.8 s | 200 | 7,946 tok, 1.63 s | 500 `input (8571 tokens) …` |
| 16384 | **12,096 MiB** | 1.6 s | 200 | 15,892 tok, 4.00 s | 500 `input (16763 tokens) …` |

- The plan's estimates (14 / 20 / 32 GiB) were far too high and have been replaced with these measurements plus headroom.
- The pinned server rejects over-limit input with **HTTP 500**, not 400, before running inference. The miner maps this to `context_length_exceeded`, and the message includes the exact token count.
- The first request after loading takes about 3 s (warm-up). Later small requests take about 0.16 s.

## Qwen2.5-1.5B-Instruct Q4_K_M (`6a1a2eb6…`, HF revision `91cad511…`)

Launch: `-ngl 99 -c N -b 512 -ub 128 --parallel 1 --no-context-shift`.

| Profile | Peak VRAM above baseline | Near-limit request (input + 256 output) | Over-limit request |
|---|---:|---|---|
| 4096 | **1,334 MiB** | 3,716 tok in, 200 | 400 `request (4194 tokens) exceeds the available context size (4096 tokens)…` |
| 8192 | **1,446 MiB** | 7,689 tok in, 200 | 400 |
| 16384 | **1,672 MiB** | 15,635 tok in, 200 | 400 |

Chat behaviour verified at the pinned commit:

- `POST /v1/chat/completions/input_tokens` returns `{"input_tokens":35,"object":"response.input_tokens"}`, giving an exact templated count without inference.
- Streaming chunk order: a role-only chunk, then content deltas, a chunk with `finish_reason`, a chunk with `"choices": []` carrying `usage` (when `stream_options.include_usage`), then `data: [DONE]`.
- When the client disconnects mid-stream, the server stops generating and frees the slot (`slot release … stop processing`). `/slots` then reports `is_processing: false`.

## Result

Both models at their largest profiles total **≈ 13.8 GiB**, which fits one 16 GB card alongside the desktop. Catalog memory figures are the measured peak plus about 10% headroom, rounded up.

## Clef token-count helper (`native/clef-token-count`)

The helper is built CPU-only and statically from the pinned llama.cpp `server-context` library. It loads only the vocabulary (`vocab_only`) and runs the server's own `parse_questions → parse_state → fill_task_joint` path. Build:

```sh
cmake -S native/clef-token-count -B build/clef-token-count -DCMAKE_BUILD_TYPE=Release -G Ninja
cmake --build build/clef-token-count --target clef-token-count
```

(`-DLLAMA_CPP_SOURCE_DIR=<checkout>` reuses an existing checkout at the pinned commit instead of fetching one.)

- Binary: 13.8 MB, needs only libc and libstdc++. Startup takes 0.22 s and peaks at 97 MiB RSS, without touching the GPU.
- `scripts/qualify/compare_clef_counts.py` ran 18 cases against the pinned server (`data/clef-count-compare.txt`): **18/18 exact matches**. The cases cover string, object and array state; all three question types; unicode; marker text in the input; 16 questions; 64 options; inputs near 4k and 16k; over-limit input (matched against the count in the server's rejection); and invalid requests (same error text). Counting takes 0.2–25 ms per request.
- The server accepts a one-option `choice`, so the pool must enforce its own 2-option minimum.

## End-to-end: pool-server + pool-miner (2026-10-08)

Release builds on this machine, local auth, using the commands from the README quickstart.

- The miner detected the RTX 5070 Ti (14,404 MiB free, budget 14,148 MiB) and planned `clef` cuda-8192 (11,264 MiB) + `qwen2.5-1.5b-instruct` cuda-16384 (2,048 MiB). It installed `b11374` from `runtime/manifest.json` (both archives hash-verified, version check `commit b92761a51`) and had both models ready about 7 s after start, with cached weights.
- Downloads: Qwen took 1 m 42 s for a full download. An interrupted download resumed at byte 18,535,380 and the result's SHA-256 matched. Importing Clef with `pool-miner download clef --import` hard-linked the file and verified it in 4.3 s.
- `POST /v1/systemone` (README example): 200, `x-model-profile: cuda-8192`. Answers were route = billing (0.987), angry noul 0.098, urgency score 1.67, with 320 input tokens. In the miner selftest, preflight counts equalled the runtime's `usage` for Clef (226 = 226) and Qwen (35 = 35).
- `POST /v1/chat/completions` streaming (README example): 43 SSE events with content, a usage chunk (`prompt_tokens` 37, `completion_tokens` 40), then `[DONE]`. Non-streaming chat: 200.
- A 9,139-token Clef request with only cuda-8192 loaded got 503 `context_unavailable`.
- A client disconnecting mid-stream freed the slot, and the next request was served. Three concurrent Clef requests were all served in order.
- Killing the llama-server child set `ready_miners` to 0 until it restarted 5 s later; it then served again. After a pool restart, the miner reconnected with backoffs of 1.7 s, 2.7 s and 4.7 s on a new session.
- On Ctrl-C the miner drained: an active stream finished (1,202 chunks + `[DONE]`), new requests got `no_miner_available`, and it exited with no llama-server or helper left. GPU memory went back to baseline.

## Clef with weights offloaded to system RAM (2026-10-08)

`scripts/qualify/measure.py --ngl N` keeps N of Clef's 33 offloadable layers on the GPU and the rest in system RAM. RAM is the sum of llama.cpp's host buffers (`CPU_Mapped` weights and `CUDA_Host` buffers). Clef reads each request in one batch, and by default (`--op-offload`) llama.cpp streams the RAM-resident weights over PCIe to the GPU for large batches, so offloading costs little time on this PCIe 5.0 x16 card. Slower links (PCIe 3.0, x4/x8 slots, laptops) will add more per request.

| GPU layers | Context | VRAM peak | Host RAM | Small request | Near-limit request |
|---:|---:|---:|---:|---:|---|
| 33 (all) | 4096 | 9,324 MiB | — | ~0.16 s | 0.73 s |
| 24 | 4096 | 7,510 MiB | 4,480 MiB | 0.255 s | 3973 tok, 0.834 s |
| 16 | 4096 | 5,768 MiB | 6,234 MiB | 0.339 s | 3973 tok, 0.906 s |
| 8 | 4096 | 4,024 MiB | 7,987 MiB | 0.44 s | 3973 tok, 0.99 s |
| 0 | 4096 | 1,428 MiB | 9,520 MiB | 0.479 s | 3973 tok, 1.07 s |
| 33 (all) | 8192 | 10,184 MiB | — | ~0.16 s | 1.63 s |
| 24 | 8192 | 8,530 MiB | 4,928 MiB | 0.256 s | 7946 tok, 1.72 s |
| 16 | 8192 | 6,788 MiB | 6,682 MiB | 0.327 s | 7946 tok, 1.873 s |
| 8 | 8192 | 5,044 MiB | 8,436 MiB | 0.412 s | 7946 tok, 1.998 s |
| 0 | 8192 | 2,448 MiB | 9,968 MiB | 0.466 s | 7946 tok, 1.992 s |
| 33 (all) | 16384 | 12,096 MiB | — | ~0.16 s | 4.0 s |
| 24 | 16384 | 10,798 MiB | 6,209 MiB | 0.257 s | 15892 tok, 4.042 s |
| 16 | 16384 | 9,056 MiB | 7,963 MiB | 0.339 s | 15892 tok, 4.24 s |
| 8 | 16384 | 7,308 MiB | 9,716 MiB | 0.409 s | 15892 tok, 4.343 s |
| 0 | 16384 | 4,698 MiB | 11,248 MiB | 0.465 s | 15892 tok, 4.367 s |

The catalog publishes these as `cuda-<context>-gpu<layers>` profiles, with VRAM (`memory_mib`) and RAM (`host_memory_mib`) at the measured value plus about 10%. The miner uses them only when no full-GPU profile fits.

End to end: with another process holding 9.3 GB of VRAM (5.4 GB free), a fresh `pool-miner --models clef` planned `cuda-8192-gpu0` (2,816 MiB VRAM + 11,008 MiB RAM). Its probe passed in 3.4 s. The README request returned billing 0.987 / angry 0.098 / urgency 1.67 (identical to full GPU) in 0.40 s, and a 7,741-token request took 2.4 s.
