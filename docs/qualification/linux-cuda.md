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
