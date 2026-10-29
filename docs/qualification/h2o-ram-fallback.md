# H2O automatic RAM fallback on a 4 GB laptop GPU

Historical BF16 measurements: the `h2o` model id has since been removed.
The current model is `h2o-q8`; use the current README for commands and memory
requirements. These measurements retain the weights and settings actually tested.

Date: 2026-10-14. Question: does a miner with a 4 GB GPU serve `h2o` when none of its published profiles fits, using the automatic RAM fallback?

- **Machine**: kachigar, RTX 3050 Ti Laptop 4 GB (2.66 GB free with the desktop running), Ryzen 7 6800H, 30 GB RAM, CachyOS, driver 615.71.09.
- **Miner**: pool-miner built from this change, `pool-miner --pool https://pool.example.com --models h2o --yes`, serving live pool traffic.

The plan chose the smallest published profile with all weights in system RAM:

```
h2o  cuda-8192  context 8192  1437 MiB (estimate) + 11264 MiB RAM (estimate), automatic RAM fallback (-ngl 0)
```

| | estimate | measured |
|---|---|---|
| llama-server VRAM | 1437 MiB | about 470 MiB (1782 MiB used on the GPU, 1315 MiB of it by the desktop) |
| llama-server RAM | 11264 MiB | 8.6-9.2 GB RSS |
| 8192-token startup probe | must pass within 120 s | 12.4 s |
| live throughput, 90 s | | 472 computed prompt tokens/s, 0.36 questions/s, GPU 59% |

llama-server ran with `-ngl 0 -c 8194 --parallel 1 -ub 2048`; the pool listed the miner as `cuda-8192`. Its main thread used 99% of a core: with the weights in RAM every prompt chunk streams them to the GPU, so the laptop answers roughly a fifth of what an RTX 4000 SFF Ada answers. The estimates are conservative on both sides.
