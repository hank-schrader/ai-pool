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

## Findings, fixed in v0.1.1

1. **A static budget cannot predict whether a profile runs.** The 70% budget (11,468 MiB) let the plan pick metal-8192, which failed only on its first full-size request. **Fix:** the miner now probes every profile with a request filling about 85% of its context before advertising it. An automatically chosen profile that fails steps down to the next smaller one; a forced `--profile`/`--context` stays unavailable with a hint.
2. **After a Metal compute error, llama-server stays broken.** **Fix:** a job that gets a runtime failure makes the supervisor restart that server, and the restarted server is probed again.
3. **The pool waited out the 15 s queue timeout** when its only miner failed with a retryable error. **Fix:** with no other eligible miner, the client gets the miner's error at once (`502 miner_failed`). Regression test: `fails_fast_when_no_other_miner_can_retry`.

## Re-test with v0.1.1 (automatic plan, no overrides)

- `install.sh` upgraded the Mac to the v0.1.1 release.
- Clef: the plan chose metal-8192. Then `metal-8192 failed its startup probe (runtime HTTP 500: Compute error.); stepping down to metal-4096`, then `metal-4096 passed its 4096-token probe in 19.8s`, and it connected. README request through `https://ai.metalloobrabotka.online`: HTTP 200 in 1.8 s, `x-model-profile: metal-4096`, billing 0.987. A 5,000-token request got `503 context_unavailable` in 0.5 s.
- Qwen: `metal-16384 passed its 16384-token probe in 21.5s`. A streamed chat request returned a correct answer ending in `[DONE]`.

Runtime status for macOS stays `qualified: false` until cancellation and crash recovery are also checked on the Mac. Metal memory figures in the catalog are still estimates. The probe now protects against wrong estimates, at the cost of one failed attempt (about 15 s) at startup.

## Clef 8192 with other apps closed

Chrome, ghostty and Tailscale (including its network extension) were quit, bringing memory in hard use to 4.0 GiB (wired 1.0, apps 2.3, compressed 0.7) with 88% free. metal-8192 **still failed its probe with the same `Compute error`**, and metal-4096 passed in 19.8 s. So the 8192 failure on a 16 GB M1 Pro is a hard limit of this hardware with runtime b11374, not pressure from other apps. On 16 GB Macs, Clef Q8_0 runs at 4096. Larger contexts need more unified memory, or a smaller quantization such as Clef Q4_K_M (not in the catalog yet).
