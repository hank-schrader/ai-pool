# Elephant-1 Q8 cutover

Version 0.2.11 replaces the served `h2o-q8` model with `elephant-1-q8`.
The Qwen chat model and its artifact/runtime pins remain unchanged. Old H2O
artifacts are not automatically deleted from a miner's cache.

## Model and runtime

- Original: [Base Labs Elephant-1](https://huggingface.co/baselabs/elephant-1), MIT, 9B.
- Decision-wired Q8 GGUF: `debackerl/elephant-1-Q8_0-GGUF`.
- Revision: `7bb3494b7aa1a790fca6e84b1de06e7e6a670e97`.
- File: `elephant-1.Q8_0.gguf`, 9,527,504,224 bytes.
- SHA256: `652c102967efa9a1adf9c63ebaada547fba189888ca0680944aae595398fdf9d`.
- Runtime: llama.cpp b11858, commit `30322097581f70126705570def8810013787af47`.

The GGUF header was inspected directly. It contains `qwen35.decision.type=openjev`,
identity temperatures, the named `systemone` template with explicit option labels
and newlines, and `qwen35.attention.causal=false`. A plain chat GGUF is insufficient.
This is a third-party quantization/packaging of the original model, not BF16
reference inference or a claim of bitwise-identical probabilities. The runtime's
native OpenJev mask/prompt execution is used; no claim of equivalence to qev's
shared-prefix block mask is made.

## Memory

Weights occupy about 8.87 GiB. The published one-slot CUDA profiles estimate
12 GiB at 4096 tokens and 14 GiB at 8192 tokens. These are planning budgets,
not measured GPU peaks. The owner has a 16 GiB miner; the development PC has
only 4 GiB VRAM, so it cannot qualify full-GPU placement. The miner must answer
a near-full-context probe before advertising availability. New runtime targets
remain unqualified and require `--allow-unqualified-runtime` until measured.
Automatic RAM fallback is disabled. Mammoth-1 Q8 weights alone are about
28.60 GB, exceeding 16 GiB before working memory.

An automatically selected 8192-token profile may step down to the smaller
published full-GPU context if its startup probe fails. Explicit `--profile` or
`--context` requests remain exact and never change silently.

## API and verification

`POST /v1/systemone` uses the existing state/questions interface. The text-only
contract `openjev-text-v1` accepts up to 16 questions, 2–26 choice options,
2–10 score levels, and noul. Image input and streaming are refused.
Native label readout returns probabilities without generating a chat answer.

The bundled helper retains its historical `clef-token-count` filename so the
installers need no binary-name migration. It now supports OpenJev's independent
questions and is built from the same commit as Elephant's server. Admission
counts the longest prompt; completion usage sums all question prompts. The
existing `pool-miner selftest` checks that summed counts equal runtime usage.

Miners negotiate `openjev-v1`. Older clients cannot receive the unknown backend;
an incompatible default catalog returns HTTP 426 `miner_outdated`.

## Owner setup

Install v0.2.11 or newer, then run:

```sh
pool-miner --pool https://ai.metalloobrabotka.online \
  --models elephant-1-q8 --yes --allow-unqualified-runtime
```

The pool server holds model metadata and requests only. The miner downloads
the pinned weights directly from Hugging Face and verifies SHA256. With no
connected Elephant miner, the pool must advertise zero ready capacity.
