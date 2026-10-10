# H2O-Lightning-4B BF16 (refactor-tool) with weights in system RAM

Date: 2026-10-10. Question: how fast does refactor-tool's model answer a 40k-token question on a 4 GB laptop GPU?

`scripts/qualify/h2o_refactor_tool_bench.py` reproduces `refactor-tool/crates/llm-connector` exactly:

- **Server flags** (`engine.rs`): context (40960+2)×3 slots, `-np 3 -b 2048 -ub 512 -fa on -ctk f16 -ctv f16 --no-context-shift --cache-ram 1024 --ctx-checkpoints 8`. Only `-ngl` changes, because the 8.4 GB of BF16 weights cannot sit on a 4 GB GPU.
- **Prompt** (`lightning_prompt.rs`): ChatML system/user turns, `record: … question: … options: A) …`, thinking disabled, `Answer:` prefill.
- **Requests** (`lightning.rs`, `lightning_transport.rs`): the startup label check, `/tokenize`, then `/completion` with the token ids, the full `[id,false]` logit-bias mask over the 248,320-token vocabulary (3.8 MB body), `n_predict 1`, `n_probs`, `samplers []`, `post_sampling_probs`, `cache_prompt`. Default policy `single` means one forward pass per question.

The record is this repository's Rust sources and docs, trimmed so the prompt is 40,911 tokens (limit 40,960). Model `h2o-lightning-4b-672dc01e.BF16.gguf`, SHA-256 `74ac588b…`, llama.cpp b11374 (`b92761a`).

| Machine | Setup | Cold 40,911-token question | New question, same record | Same question again | Peak VRAM | Peak server RSS |
|---|---|---:|---:|---:|---:|---:|
| RTX 5070 Ti 16 GB, PCIe 5.0 x16 | `-ngl 99` (refactor-tool today) | **8.0 s** | 0.5 s | 0.5 s | 12.4 GB | 8.5 GB |
| kachigar: RTX 3050 Ti Laptop 4 GB, PCIe 4.0 x16, 30 GB RAM | `-ngl 0` | **92.7 s** (444 tokens/s) | **2.8 s** | 2.7 s | 0.77 GB | 13.1 GB |

- Answers agree across machines: "rust" 0.913 vs 0.908; the yes/no question 0.493 vs 0.471.
- Follow-up questions about the same record re-evaluate only about 500 tokens. The hybrid model (8 attention + 24 recurrent layers) restores the last context checkpoint, so a cached record costs about 2.5 s per question on the laptop instead of 92 s.
- On the desktop GPU with `-ngl 0`, a microbatch of 2048 instead of 512 processed 32k-token prompts 2.4× faster (12.6 s vs 30.5 s) for 0.3 GB more VRAM. It was not measured on the laptop.
