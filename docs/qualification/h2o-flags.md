# H2O llama-server flags on a 16 GB miner

Date: 2026-10-12. Question: do 2048-token prompt chunks (`-ub 2048`) and no RAM prompt cache (`--cache-ram 0`) let an h2o miner answer more questions than refactor-tool's `-ub 512 --cache-ram 1024`?

- **Machine**: Vast.ai RTX A4000 16 GB, Core i7-14700, Ubuntu 24.04 container, profile `cuda-40960-x4` (4 slots, 8 jobs with prefetch). pool-miner and pool-server v0.2.1 on the same box, so only the llama-server flags change; they were appended through a `--llama-server` wrapper.
- **Workload**: 40 records cut from this repository's Rust sources and docs, 1,000-12,000 tokens each, with 1-8 questions per record (half `noul`, half `choice` with 4-5 options), 186 questions in all, seed 20261012. 12 clients send the records at once, so the miner's 8 jobs always queue on its 4 slots, as on the public pool.

| flags | wall | questions/s | median / p95 latency | VRAM |
|---|---|---|---|---|
| `-ub 512 --cache-ram 1024` | 411 s | 0.452 | 118 s / 256 s | 13,728 MiB |
| `-ub 2048` | 418 s | 0.445 | 103 s / 264 s | 14,058 MiB |
| `-ub 2048 --cache-ram 0` | 391 s | 0.476 | 101 s / 220 s | 14,058 MiB |

All 186 questions were answered in every run. One run per setting.

- The larger chunks alone change nothing. Without the RAM cache the miner answers 5% more questions: saving and restoring ~100 MB prompt states costs more than the reuse it buys when several jobs share 4 slots.
- `-ub 2048` adds 330 MiB, within the `cuda-40960-x4` estimate of 14,336 MiB. The 40,960-token probe takes the same time with either setting (13.7-14.6 s).
- Higher GPU utilisation is not a throughput measure here: without the cache the GPU recomputes prefixes it used to restore. On the live pool, GPU use rose from ~80% to ~90% with the new flags while answered questions per second stayed within the noise of the changing request mix.
- Also tried on a Vast RTX 4080 host whose kernel only gives transparent huge pages on request: `GLIBC_TUNABLES=glibc.malloc.hugetlb=1` cut llama-server's page faults from 251,000/s to 8,000/s but gave no measurable speedup.

## One slot per request

Date: 2026-10-14. With `--cache-ram 0`, a prefetched job usually took a request's slot between two of its questions, so every question recomputed the shared system prompt and record (2-4% of prompt tokens came from cache on live miners). The engine now tokenizes a request's questions, takes one free slot and sends all of them with that `id_slot`, then frees it.

`pool-miner bench --models h2o --profile h2o=cuda-40960-x6 --duration 120` on a Vast.ai RTX PRO 4000 Blackwell 24 GB (105 W cap, Core Ultra 9 285K), same seeded workload (1-8 questions per request, 300-40,000-token records), 12 requests in flight:

| engine | requests/s | questions/s | prompt tokens/s | p50 / p95 latency | VRAM |
|---|---|---|---|---|---|
| any idle slot per question | 0.30 | 1.58 | 4,665 | 30.8 s / 76.1 s | 16,806 MiB |
| one slot per request | 0.57 | 2.81 | 10,791 | 18.6 s / 43.4 s | 16,802 MiB |

No errors in either run. Requests with a single question behave as before; the gain grows with the number of questions per record.

## No context checkpoints

Date: 2026-10-14. h2o is a hybrid model (`qwen35`: gated delta net layers plus attention). For such models llama-server ends every prompt with a separate 4-token decode and copies the recurrent state into a context checkpoint, so a later prompt that shares a prefix can restore it. The pool's client sends one question per request, so on live miners only 1-3% of prompt tokens came from a checkpoint.

Local RTX 5070 Ti 16 GB with a Ryzen 9 9950X, runtime b11374, `cuda-40960-x4`. A load generator sent the engine's own `/completion` body (label bias, `n_probs`, one pinned slot per worker, 4 workers) with one question per request: 40% of prompts 250-700 tokens, 60% 900-3,300, about 1,470 on average, behind a shared system prompt. 75 s per run after a 10 s warm-up:

| flags | questions/s | prompt tokens/s |
|---|---|---|
| `--ctx-checkpoints 8` (two runs) | 3.98 / 3.95 | 5,866 / 5,844 |
| `--ctx-checkpoints 0` (two runs) | 4.29 / 4.35 | 6,289 / 6,340 |

- 9% more questions answered. With checkpoints off the server processes prompts at 93-94% of `llama-bench`'s pp1450 rate on this card (6,743 tokens/s; `llama-batched-bench` with 2-8 prompts in parallel: 6,700-7,500).
- The same 30 prompts answered one at a time: label probabilities differ by at most 0.0055 (mean 0.0013), no top answer changed; `cuda-40960-x3` and `x4` differ by as much with checkpoints on. Repeated runs with the same flags are identical.
- The cost: a request's later questions can no longer restore its record from a checkpoint (llama-server makes them 4 and 4 + 2,048 tokens before each prompt's end), so requests with several questions on a long record should lose most of the one-slot-per-request gain above (not measured). Turn checkpoints back on if clients start sending several questions per request.
- Not worth changing, measured on the same card: `-b`/`-ub` from 512 to 4096 (within 3%), more slots (`x3` vs `x4`: 1%), `--kv-unified` (equal or slower). llama-server's probability readout computes a softmax over all 248,320 tokens and sorts them for every question (13 ms together on the 9950X). Requesting pre-sampling probabilities instead, which skips the full sort but ignores the label bias and so gives unusable answers, ran 4% faster; that is only an indication of what a llama.cpp change could gain.
- On the live pool the two identical RTX PRO 4000s on one Vast host ran side by side, one with checkpoints and one without, for three 2-3 minute windows under the same traffic: the one without completed 19-28% more prompt tokens and kept its GPU 95-97% busy instead of 69-86%. Those windows were not always saturated, so the figure is indicative.
