# Qualification: Linux aarch64 CPU on Asahi

Date: 2026-10-29. Model: `h2o-q8`, Q8_0, SHA-256
`4d4f7f5e601fb78d726761d9aa2b1ad0dc17e75fd65b0b59dd15b31df0837f84`.
The former BF16 model `h2o` is no longer served. The default ARM Linux path is
CPU-only, with no GPU driver or device-name requirement.

## Machine and runtime

Apple M1 Pro, six performance and two efficiency cores, 15,580 MiB RAM.
Fedora Asahi Remix 44, kernel `7.2.12-402.asahi.fc44.aarch64+16k`, **16,384-byte
pages**, glibc 2.43. About 11.3 GiB RAM was available before loading. All work
used the user's home directory, with no sudo or production services.

Pinned runtime: llama.cpp b11374, commit
`b92761a515ea31e852e7fbc1fad5f874b46f3718`.

| Archive | Bytes | SHA-256 |
|---|---:|---|
| CPU: `llama-b11374-bin-ubuntu-arm64.tar.gz` | 13,688,667 | `5541cbbcf064cb065beee4830ba454177215166361207cc1a94508d807d76a00` |
| Historical comparison: `llama-b11374-bin-ubuntu-vulkan-arm64.tar.gz` | 24,849,096 | `63e6139f979ba669cc734b4500f9c98525b972c028478509380c237cec0a163b` |

Both archives were downloaded and hashed. The CPU archive contains all eight
ARM CPU backend variants, including dotprod and i8mm builds. Its selected
`libggml-cpu-armv8.2_2.so` is byte-for-byte identical to the Vulkan archive's;
the M1 Pro reports NEON, ARM_FMA, FP16_VA, DOTPROD, OPENMP and REPACK. The CPU
archive's loader resolves libgomp, OpenSSL 3, libstdc++ and glibc, with **no
libvulkan dependency or bundled Vulkan backend**. `readelf --version-info`
across the shared libraries gives the same maximum GLIBC_2.38 requirement.
The downloaded executables and native counting helper run on the 16K kernel.

## CPU defaults and scope

`Accelerator::Cpu` and the `cpu` catalog feature replace the proposed Vulkan
path. The manifest permits CPU only for `aarch64-unknown-linux-gnu`. The server
accepts CPU miner registration and publishes their capacities normally.
No Apple model or GPU name is matched: other Asahi Apple Silicon machines,
Ampere, Graviton and other Linux aarch64 CPUs can try the same profiles.

The memory budget is real Linux MemAvailable minus 2,048 MiB, with unknown
memory rejected. `memory_mib` covers the entire CPU process; there is no second
host-memory reservation. CPU launch flags are selected per accelerator:

```text
-ngl 0 --device none --no-op-offload --load-mode none
-b 512 -ub 512 -t <performance cores> -tb <performance cores>
-fa on -ctk f16 -ctv f16 --no-context-shift --cache-ram 0 --ctx-checkpoints 0
```

b11374 calls no mmap `--load-mode none`; `--no-mmap` is not accepted by this
pin. On this laptop sysfs `cpu_capacity` is 485 for two cores and 1024 for six,
so both thread counts are six. Detection falls back to maximum CPU frequency
when capacity is absent or uniform, then all available cores if both metrics
are missing or uniform.
Uniform hosts use all available cores. The count is capped by the OS's
available parallelism. Unit tests cover heterogeneous, uniform and missing
metrics, and the CPU flags; the CUDA flags remain 2,048-token batches.

## Measuring the CPU alternatives

The initial CPU-only mmap run reached **9,345 MiB RSS**, because it retained
4,264 MiB of CPU_Mapped weights alongside 4,260 MiB CPU_REPACK. No mmap replaces
that copy. RSS here includes file-backed resident pages; it is not an anonymous
memory-only estimate. All runs sampled `/proc/<pid>/status` every 50 ms and
used the Asahi GEM allocation tracer. Final CPU-only measurements have zero GEM
allocations; no GPU heap is omitted from their RSS figure.

Single exploratory requests, a 4,096-token context and one slot, realistic
billing questions with exactly the listed prompt lengths:

| Archive / loading | Threads | Batch / microbatch | 800-token latency | 2,048-token latency | 4,096-token latency | Peak RSS |
|---|---:|---:|---:|---:|---:|---:|
| Vulkan archive, CPU, no mmap | 8 | 2048 | 6.32 s | 16.96 s | 36.93 s | 5,667 MiB |
| Vulkan archive, CPU, no mmap | 6 | 2048 | 5.95 s | 17.17 s | 38.71 s | 5,667 MiB |
| Vulkan archive, CPU, no mmap | 6 | 512 | 5.70 s | 16.12 s | 37.06 s | 5,355 MiB |
| CPU archive, no mmap | 6 | 512 | 5.97 s | 15.93 s | 36.23 s | 5,316 MiB |
| CPU archive, no mmap | 8 | 512 | 6.29 s | 16.71 s | 36.50 s | 5,316 MiB |

512-token batches improve cancellation granularity, reduce memory, and show
no throughput penalty here. Six threads are 4–5% faster at 800/2,048 tokens
and effectively equal at 4,096. CPU and Vulkan archives have the same CPU
backend and comparable timings, so the smaller CPU archive is used.
These are individual runs, without statistical confidence bounds.
Raw data: [CPU alternatives](data/h2o-q8-asahi-cpu-candidates.json). Their
`direct_disconnect_to_idle_seconds` fields used the frequent-polling method
explained below and are not valid cancellation qualification measurements.

## Published profiles and full-context checks

`scripts/qualify/measure_h2o.py` verifies labels, a known small answer, every
slot at the exact prompt limit, and context + 1,024 rejection. It uses the
pool's positive label bias, one-label readout and pinned sampling settings.
CPU RSS includes model loading, repacking and inference. The GEM tracer checks
for driver allocations outside RSS; summing the separate peaks is conservative.

| Profile | Slots | Peak RSS | Peak GEM | Published RAM with headroom | Full-context latency / prompt speed |
|---|---:|---:|---:|---:|---|
| `cpu-4096` | 1 | 5,308 MiB | 0 | 6,144 MiB, measured | 37.13 s / 110.39 tokens/s |
| `cpu-8192` | 1 | 5,444 MiB | 0 | 6,144 MiB, measured | 91.17 s / 89.87 tokens/s |

The CPU alternatives reached 5,316 MiB at 4,096 context; the mixed miner
benchmark reached 5,447 MiB at 8,192. These slightly higher peaks also fit the
same reservation. The reservation follows the CUDA convention: about 10% above measured peak,
rounded up. The small known answer took 0.62–0.63 s. Both boundary requests
returned every prompt token evaluated and `truncated: false`; the over-limit
requests returned HTTP 400. No multi-slot CPU profile is published. Automatic
startup probes the 8,192-token profile first and can step down to 4,096, even
when both memory figures round to the same value. Unknown CPUs must pass the
same probe before advertising capacity.

Reproduce (repeat at 4,096):

```sh
gcc -shared -fPIC -O2 -o asahi-memory.so scripts/qualify/asahi_memory.c -ldl -pthread
python3 scripts/qualify/measure_h2o.py --server /path/to/llama-server \
  --model /path/to/h2o-lightning-4b-672dc01e.Q8_0.gguf --ctx 8192 --ngl 0 \
  --cpu-only --no-mmap --threads 6 --batch 512 \
  --memory-trace "$PWD/asahi-memory.so" --log cpu-8192.log
```

Raw data: [final CPU profiles](data/h2o-q8-asahi-cpu-memory.json),
[historical Vulkan measurements](data/h2o-q8-asahi.jsonl). The historical
RAM/Vulkan profiles peaked at 5,459/5,445 MiB (GEM + RSS), so CPU no mmap has a
comparable footprint and a faster answer path. Those Vulkan profiles are no
longer in the catalog; ash and Vulkan detection have been removed.

## Cancellation and qualification

The miner does not call a separate runtime cancel endpoint. A pool cancel
aborts the job task; `tokio::time::timeout` drops the same inference future on
deadline. The pending reqwest request is dropped, closing its TCP connection
even before response headers. A real-socket unit test keeps the client's pool
alive and verifies EOF after cancellation.

Two issues explain the initial failed cancellation checks:

1. llama-server applies cancellation between decode batches. Its 2,048-token
   CPU batch took about 17–20 s, so a disconnect could not promptly free a slot.
2. The old check polled `/slots` every 250 ms. In pinned `server-queue.cpp`,
   `recv_with_timeout` restarts its one-second wait whenever any task produces
   a result; `server_response_reader::next` checks `req.should_stop` only on
   timeout. Frequent status requests could therefore prevent the disconnect
   check altogether. This is an upstream polling limitation, not evidence that
   the miner kept its HTTP request alive.

The replacement `scripts/qualify/cancel_h2o.py` reads logs without making
runtime status requests during cancellation, then checks `/slots` once after
release. On a 6,001-token prompt, disconnecting one second into prefill posted
a native cancel after about one second with either batch setting:

| Batch / microbatch | Disconnect to native cancel | Disconnect to slot release |
|---|---:|---:|
| 2048 | 0.98 s | **16.55 s** |
| 512 | 0.99 s | **2.70 s** |

The local pool/miner test uses the same log-based observation. A real
`/v1/systemone` client disconnect released its slot in **2.72 s**. Keeping the
client connected until the 20 s deadline returned HTTP 504 in **20.01 s** and
released the runtime slot **1.21 s later**. Both were followed by successful
requests. These are measured delays at the tested prompt positions, not a
fixed cancellation guarantee for other hardware or deeper attention contexts.

The local pool selected `cpu-8192`, verified A–P labels, and passed its startup
probe in **76.7–78.5 s**. A real two-question request returned HTTP 200 in
**1.44 s**, 178 computed tokens, sky-blue noul **0.9155**, billing probability
**0.9507**. Killing only its runtime withdrew capacity, restarted after five
seconds and re-probed; recovery including a successful request took **83.08 s**.
Restarting the pool reconnected and served again in **2.41 s**. Ctrl-C with a
request active drained it with HTTP 200, exited 0 and reaped the runtime.

Memory, limits, counts/usage, sane answers, cancellation/deadline, child failure
recovery, reconnect, and graceful shutdown passed. The CPU target is now
**`qualified: true`**, exercised on this Asahi machine; other ARM machines are
still protected by their own startup probe. Frequent `/slots` polling remains
an upstream limitation of b11374 and should be avoided in monitoring.

Raw data: [pool integration](data/h2o-q8-asahi-cpu-e2e.json),
[direct batch comparison](data/h2o-q8-asahi-cpu-cancel.json). The previous
[failed polling check](data/h2o-q8-asahi-e2e.json) is retained as historical
measurement evidence, superseded by the corrected method above.

## 40,960-token candidate

A temporary, unpublished `cpu-40960` profile passed its initial estimated RAM
fit and loaded. Its actual 85%-length miner startup probe failed with
`no answer within 120s`; wall time including cache verification, loading and
cleanup was **140.79 s**, exit 1. It was never advertised and is not in the
catalog. The previous Vulkan 40,960 candidate also failed its 120 s probe
(137.04 s total). Only the published CPU 4,096/8,192 profiles are qualified;
40,960-token CUDA profiles remain available to CUDA miners.

Raw data: [CPU candidate](data/h2o-q8-asahi-cpu-40960.json),
[historical Vulkan candidate](data/h2o-q8-asahi-40960.json).

## Fixed-size realistic pool requests

`pool-miner bench` mixes seeded record lengths, so
`scripts/qualify/bench_h2o_sizes.py` sends requests through a real local pool and
miner and reads llama-server's prompt timers. Three different support records
per size prevent whole-prompt cache reuse; each has one billing/shipping/technical
choice question. States are 715, 1,963 and 4,011 tokens, plus 85 template/question
tokens. Latency includes the public HTTP request, admission token counting,
dispatch and label readout, but excludes startup. Questions/s uses a continuously
occupied full slot count of one. The final path uses the default `cpu-8192`
profile, with no wrapper overrides of its loading, batches or thread defaults.

Generation is a separate `llama-bench` tg32 diagnostic after prefill at the
same 800/2,048/4,096 depths, three repeats. H2O itself reads one label and does
not generate prose. The prefill is outside the generation timer. Historical
Vulkan and mmap CPU rows used a 4,096-token context and eight threads; final
CPU rows use 8,192 context, six threads, 512 batches and no mmap.

| Path | Prompt tokens | Prompt tokens/s | Generation tokens/s (tg32) | Median request latency | Questions/s, full one-slot load |
|---|---:|---:|---:|---:|---:|
| **Final default CPU** | 800 | 141.8 | 17.67 | 5.66 s | 0.1764 |
| Historical full Vulkan | 800 | 90.3 | 12.40 | 8.92 s | 0.1122 |
| Historical CPU mmap, eight threads | 800 | 128.3 | 7.93 | 6.27 s | 0.1596 |
| Historical RAM/Vulkan | 800 | 91.0 | 6.63 | 8.81 s | 0.1134 |
| **Final default CPU** | 2,048 | 129.0 | 17.14 | 15.85 s | 0.0628 |
| Historical full Vulkan | 2,048 | 100.5 | 12.16 | 20.46 s | 0.0489 |
| Historical CPU mmap, eight threads | 2,048 | 121.1 | 7.50 | 16.94 s | 0.0590 |
| Historical RAM/Vulkan | 2,048 | 104.4 | 6.95 | 19.68 s | 0.0509 |
| **Final default CPU** | 4,096 | 113.7 | 15.90 | 36.11 s | 0.0277 |
| Historical full Vulkan | 4,096 | 96.4 | 11.57 | 42.60 s | 0.0235 |
| Historical CPU mmap, eight threads | 4,096 | 109.7 | 6.77 | 37.51 s | 0.0268 |
| Historical RAM/Vulkan | 4,096 | 100.8 | 6.50 | 40.65 s | 0.0246 |

All **9/9** final questions were correct with no cache reuse or errors. Final
CPU latency is 15–37% lower than full Vulkan, and 11–36% lower than RAM/Vulkan.
The separate generation diagnostic also favors the final CPU defaults.

All final questions must complete correctly and compute every prompt token.
These are three-request samples, not long public-pool capacity tests. AC power,
one model/benchmark at a time. Raw data: [final requests](data/h2o-q8-asahi-cpu-sizes.json),
[final generation](data/h2o-q8-asahi-cpu-generation.json),
[historical requests](data/h2o-q8-asahi-sizes.json) and
[historical generation](data/h2o-q8-asahi-generation.json).

```sh
python3 scripts/qualify/bench_h2o_sizes.py --catalog config/models.json \
  --server /path/to/llama-server --bench /path/to/llama-bench \
  --miner /path/to/pool-miner --pool-server /path/to/pool-server \
  --weights /path/to/h2o-lightning-4b-672dc01e.Q8_0.gguf \
  --modes cpu-final --threads 6 --context 8192 --requests 3 --output cpu-sizes
```

## Installer, doctor, compatibility and validation

The native ARM release package was rebuilt from this tree, including the
pinned `clef-token-count` helper. The actual installer selected aarch64,
verified SHA256SUMS, installed under `~/.local/share/ai-pool/v0.2.7` and linked
the binaries. Only curl's two GitHub release-asset downloads were redirected
to the local release fixture. Official release installation remains phase 2.
The managed CPU runtime was fetched from GitHub normally and hash/commit
verified; the previous Vulkan cache marker was replaced. Weights were imported
and SHA-256 verified.

The installed binary also ran against a local pool **without**
`--allow-unqualified-runtime`: HTTP 200 in **1.34 s**, `cpu-8192`, the same
sane two answers and 178 prompt tokens, followed by a clean exit 0.

Final doctor output reports glibc 2.43, libgomp, OpenSSL 3, a working loader,
6/8 CPU threads, the qualified CPU runtime and the pinned helper. Available RAM
was 11,701 MiB, giving a 9,653 MiB planning budget. `list-models` shows only
`h2o-q8` as the default, with both CPU profiles fitting. No libvulkan or Vulkan
device is required.

```text
[ok] glibc: 2.43 (required >= 2.38)
[ok] accelerator: cpu ... available RAM minus 2048 MiB; 6/8 CPU threads
[ok] runtime llama-b11374 aarch64-unknown-linux-gnu (qualified)
Model    Profile    Place Jobs Req/s Q/s  In/s p50 s p95 s Err RSS
h2o-q8   cpu-8192    CPU    1    0.01 0.06 107  29.5 129.2   0 5447
```

The miner benchmark used `--models h2o-q8 --profile h2o-q8=cpu-8192 --prefetch 0
--duration 30 --json`, seed 20261012. Admissions closed at 30 s, drain at
158.65 s: **2/2 requests, 9 questions, 16,924 computed prompt tokens, zero
errors/timeouts**, 106.67 prompt tokens/s and 0.05673 questions/s. It is a small
sample. CPU finished the first record in 29.50 s and admitted another longer
record before admissions closed; the historical RAM/Vulkan run took 45.83 s
for that first record and admitted only one. The fixed-size table above holds
the question length constant for comparing execution paths.

The real released **v0.2.6** Linux binary reported `pool-miner 0.2.6` and parsed
the local pool's `offload,h2o` view, listing only `h2o-q8` and CUDA profiles.
Its filtered revision remains
`sha256:a1a19bcffa10dde0394d3bb9302b5ed6b867fb068be10d912de143211f3185d5`.
The `cpu` feature gates every CPU profile; the server accepts both catalog
revisions. Tests cover the old accelerator enum, filtered revisions, CPU
registration and inference on `cpu0`, and removed `h2o` returning the normal
HTTP 404 `model_not_found`.

`cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`,
all **146 Rust workspace tests**, and `pool-server check-config` passed locally
and natively on ARM. Frontend check, **40 tests**, production build and unchanged
committed assets passed with CI's Node 24.13.0/npm 11.6.2. No production service
was touched. All test services and benchmark children were stopped.

Raw data: [doctor](data/h2o-q8-asahi-cpu-doctor.txt),
[miner benchmark](data/h2o-q8-asahi-cpu-bench.json),
[actual v0.2.6 compatibility](data/h2o-q8-asahi-v026.txt),
[installed binary without opt-in](data/h2o-q8-asahi-cpu-final-run.json).
