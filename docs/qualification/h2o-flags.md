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
