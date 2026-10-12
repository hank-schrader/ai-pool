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
