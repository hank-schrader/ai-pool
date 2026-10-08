# Qualification: macOS arm64 + Metal

Date: 2026-10-08. Machine: MacBook Pro, Apple M1 Pro, 16 GB unified memory, macOS 27.0.1, on battery. It was used through SSH with desktop apps open, and only about 13% of memory was free.

## Install

- `scripts/install.sh` installed the `v0.1.0` release archive (`ai-pool-v0.1.0-aarch64-apple-darwin.tar.gz`, checksum verified) into `~/.local/share/ai-pool` and linked the binaries into `~/.local/bin`. No compiler or Homebrew was needed.
- `pool-miner doctor` detected `metal Apple M1 Pro`, found the CI-built `clef-token-count` through the `~/.local/bin` symlink, and fetched the catalog from `https://ai.metalloobrabotka.online` with the Mac's own miner token.
- On first `run`, the miner installed `llama-b11374-bin-macos-arm64.tar.gz` from `runtime/manifest.json` (hash verified). The version check reported `commit b92761a51`.
- Weights were imported with `pool-miner download <model> --import` (sha256 verified: Clef in 29 s, Qwen in a few seconds).

## Results through the public pool

| Model | Profile | Result |
|---|---|---|
| clef | metal-8192 (automatic plan) | **fails**: Metal `kIOGPUCommandBufferCallbackErrorOutOfMemory` on the first request, after which every request fails with `Compute error` |
| clef | metal-4096 (`--context clef=4096`) | 3 × README request: HTTP 200 in about 1.9 s each. billing 0.987, angry noul 0.101, urgency 1.66, 320 input tokens (CUDA gives 0.987 / 0.098 / 1.67) |
| qwen2.5-1.5b-instruct | metal-16384 (automatic plan) | stream: 45 events, first after 2.0 s, done in 2.4 s, usage chunk + `[DONE]`. Non-stream "17*23" answered `391` |

Direct runtime checks (`scripts/qualify`):

- `measure.py` Clef 4096: small request 1.7 s (first request), near-limit 3,973 tokens in 20.1 s (CUDA RTX 5070 Ti: 0.73 s). The over-limit request was rejected with 500 `input (4475 tokens) is too large`.
- `compare_clef_counts.py` with the release helper against the Metal server: **18/18 exact matches** (`clef-count-compare-metal.txt` on the Mac).

## Clef memory on Metal

From a verbose llama-server load at 8192: `MTL0_Mapped model buffer size = 9199.41 MiB` and `MTL0 compute buffer size = 1714.47 MiB`, so **≈ 10,913 MiB**. `MTLDevice.recommendedMaxWorkingSetSize` on this Mac is **12,124 MiB**, so the out-of-memory failure at 8192 was not the working-set limit. It came from system memory pressure: macOS could not keep 10.9 GB resident for the GPU while other apps held the rest. At 4096 the compute buffer is about half as large, and it ran.

## Findings to fix

1. **A static budget cannot predict whether a profile runs.** The miner allowed 70% of RAM (11,468 MiB) and picked metal-8192, which failed only once the first full-size request ran. The miner must probe each profile with a near-full-context request before advertising it, and step down to the next smaller profile when the probe fails.
2. **After a Metal compute error, llama-server stays broken** ("backend is in error state … recreate the backend to recover"). The miner must restart the runtime after a compute error instead of continuing to advertise it.
3. **The pool waited out the queue timeout** (15 s, `queue_timeout`, "no slot became free in time") when the only miner failed with a retryable error. With no other eligible miner, it should fail at once with the miner's error.

Runtime status for macOS remains `qualified: false` until these are fixed and re-tested. The Metal memory figures in the catalog are still estimates; the only measured Metal value is Clef at 8192, ≈ 10,913 MiB.
