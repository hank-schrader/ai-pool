# ai-pool miner

A miner serves ai-pool's models from your GPU. It connects out to the pool over
HTTPS, so it needs no open ports. The examples use `https://pool.example.com`;
replace it with the address of the pool you join. The default model is
`elephant-1-q8` (9B, Q8_0, 9.53 GB). Qwen3.8-27B UD-Q3_K_XL remains an opt-in
chat model. Both `h2o` and `h2o-q8` have been removed from the served catalog;
requests for them receive `model_not_found`.

## Requirements

| | Linux (x86_64) | Linux (aarch64) | Windows (x86_64) |
|---|---|---|---|
| Accelerator | NVIDIA, driver 570.26 or newer | ARM CPU; Apple Silicon under Asahi Linux tested | NVIDIA, driver 615 or newer (not tested yet) |
| OS | glibc 2.38+: Ubuntu 24.04+, Debian 13+, Fedora 39+, Arch | Fedora Asahi Remix 44 tested, including its 16K-page kernel; glibc 2.38+ | Windows 10/11 |
| Libraries | `libgomp`, OpenSSL 3, `libstdc++` | same; no GPU driver required | included |

Elephant's initial profiles target NVIDIA CUDA on Linux/Windows. Its runtime has
not been qualified on the owner's GPU yet; use `--allow-unqualified-runtime`
until that measurement is recorded. No Elephant CPU/Metal profile is published.

CUDA memory budgets for `elephant-1-q8` (estimates, not measured peaks). The miner subtracts
256 MiB from free GPU memory and 2 GiB from available RAM before planning:

| GPU budget | RAM budget | What the miner runs |
|---|---|---|
| 14,336 MiB | | `cuda-8192`: one request, prompts up to 8,192 tokens |
| 12,288 MiB | | `cuda-4096`: one request, prompts up to 4,096 tokens |

An otherwise free 16 GiB GPU is expected to fit Elephant Q8 with these bounded
contexts. The miner must pass its near-full-context startup probe before
advertising capacity. Automatic RAM fallback is disabled. Mammoth-1 Q8 has
28.60 GB of weights alone, so it cannot reside entirely in 16 GiB VRAM.

Elephant uses the pinned `llama-b11858` runtime and a decision-wired GGUF with
OpenJev labels, the System One prompt template, and noncausal full attention.
Its text-only API supports choice (2–26 options), score (2–10 levels) and noul
questions. Images are not enabled. Exact admission counts use the longest
question prompt; response input-token usage sums the question prompts. The
legacy-named `clef-token-count` helper now supports OpenJev too. See
[the cutover notes](docs/qualification/elephant-1.md).

Disk: about 12 GB for the weights (9.53 GB), downloads and the CUDA runtime.

The miner checks all of this itself: `pool-miner doctor` reports the accelerator,
driver, libraries, disk and cache, and prints the command that fixes each
problem. `run` and `bench` run the same checks before downloading the weights.

## 1. Install

Linux:

```sh
curl -fsSL https://raw.githubusercontent.com/hank-schrader/ai-pool/main/scripts/install.sh | sh
exec $SHELL -l          # or open a new terminal
pool-miner --version
```

The binaries go to `~/.local/share/ai-pool/<version>` and are linked into
`~/.local/bin`, which the installer adds to your shell's PATH. Pass
`--version vX.Y.Z` to pin a release or `--no-modify-path` to leave your shell
files alone (`curl … | sh -s -- --version v0.2.10`).

Windows (PowerShell):

```powershell
irm https://raw.githubusercontent.com/hank-schrader/ai-pool/main/scripts/install.ps1 | iex
# open a new terminal
pool-miner --version
```

It installs into `%LOCALAPPDATA%\ai-pool\<version>` and adds it to your user
PATH. Pin a release with `.\install.ps1 -Version v0.2.10`.

## 2. Check the machine

```sh
pool-miner doctor --pool https://pool.example.com
pool-miner list-models --pool https://pool.example.com
```

`list-models` shows each model's profiles with `fits`, `fits with weights in RAM
(estimate)` or `too large` for this machine. Fix every `[fail]` line from
`doctor` first. The usual Linux fixes:

| Distribution | Command |
|---|---|
| Ubuntu 24.04, Debian 13 | `sudo apt-get install libgomp1 libssl3t64` |
| Debian 12 | too old (glibc 2.36); use Debian 13 or Ubuntu 24.04 |
| Ubuntu 22.04 and older | too old (glibc 2.35); use Ubuntu 24.04 |
| Fedora | `sudo dnf install libgomp openssl-libs` |
| Fedora Asahi | `sudo dnf install libgomp openssl-libs libstdc++` |
| Arch, CachyOS, Manjaro | `sudo pacman -S gcc-libs openssl` |

On ARM Linux, `doctor` checks available system RAM, CPU threads, glibc 2.38+,
libgomp, OpenSSL 3 and the runtime loader. Vulkan and `vulkaninfo` are not required.

The NVIDIA driver comes from your distribution or nvidia.com; `nvidia-smi` must
work. In a container, pass the GPU through with the NVIDIA Container Toolkit.

## 3. Start mining

```sh
pool-miner --pool https://pool.example.com --models elephant-1-q8 --allow-unqualified-runtime
```

The miner picks the largest profile that fits, asks before downloading the
9.53 GB of weights, verifies their sha256, starts llama.cpp, answers a
full-length test prompt, and connects:

```
Profile plan:
  elephant-1-q8   cuda-8192   context 8192   14336 MiB (estimate), 1 cache slot
...
elephant-1-q8: cuda-8192 passed its 8192-token probe in …s
connected to https://pool.example.com (session ses_…)
```

On a GPU that cannot fit either profile, Elephant is refused. It does not
silently move the model to system RAM.

- Add `--allow-unqualified-runtime`, or answer `y` when asked; Elephant's new
  CUDA runtime has not yet been qualified on the owner's hardware.
- `--yes` skips the download question (needed without a terminal).
- `--context elephant-1-q8=8192` or `--profile elephant-1-q8=cuda-4096` forces a profile;
  `--device 1` picks the second GPU. Run one miner per GPU.
- Weights you already have can be imported instead of downloaded:
  `pool-miner download elephant-1-q8 --import /path/to/elephant-1.Q8_0.gguf`.
- Ctrl-C finishes the jobs in progress and stops; press it twice to stop at once.
  The miner reconnects by itself when the pool restarts.

## 4. Keep it running

Linux, as a systemd user service:

```sh
mkdir -p ~/.config/systemd/user
cat > ~/.config/systemd/user/pool-miner.service <<'EOF'
[Unit]
Description=ai-pool elephant-1-q8 miner
After=network-online.target

[Service]
ExecStart=%h/.local/bin/pool-miner --pool https://pool.example.com --models elephant-1-q8 --yes --allow-unqualified-runtime
KillSignal=SIGINT
TimeoutStopSec=45
Restart=on-failure
RestartSec=30

[Install]
WantedBy=default.target
EOF
systemctl --user daemon-reload
systemctl --user enable --now pool-miner
loginctl enable-linger "$USER"          # keep running after logout
journalctl --user -u pool-miner -f      # log
```

Run the miner once in a terminal first, so the weights are downloaded and
verified before the service starts.

Windows (untested): keep it running in a terminal window, or create a Task
Scheduler task that runs at log on:

```powershell
schtasks /Create /SC ONLOGON /TN ai-pool-miner /TR "pool-miner --pool https://pool.example.com --models elephant-1-q8 --yes --allow-unqualified-runtime"
```

## 5. Rented GPUs (Vast.ai and similar)

Use an Ubuntu 24.04 based CUDA image, for example
`vastai/base-image:cuda-12.8.1-auto`, SSH launch mode, at least 40 GB of disk,
and this on-start script. It runs on every start, including after a stop/start:

```bash
#!/bin/bash
POOL=https://pool.example.com
MODELS=elephant-1-q8
VERSION=v0.2.11    # pool-miner release to install

# Some hosts leave ~/.ssh with ownership sshd rejects; fix it so key logins work.
fix_ssh() { chown root:root /root /root/.ssh /root/.ssh/authorized_keys; chmod 755 /root; chmod 700 /root/.ssh; chmod 600 /root/.ssh/authorized_keys; } 2>/dev/null
fix_ssh; (sleep 60; fix_ssh) &

if ! dpkg -s curl ca-certificates libgomp1 >/dev/null 2>&1; then
    apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y curl ca-certificates libgomp1
fi
[ -d "$HOME/.local/share/ai-pool/$VERSION" ] ||
    curl -fsSL https://raw.githubusercontent.com/hank-schrader/ai-pool/main/scripts/install.sh | sh -s -- --version "$VERSION" --no-modify-path

# One miner per GPU; each restarts itself if it exits.
for gpu in $(nvidia-smi --query-gpu=index --format=csv,noheader); do
    nohup bash -c "while true; do $HOME/.local/bin/pool-miner --pool $POOL --models $MODELS --yes --device $gpu; echo \"pool-miner exited (\$?); restarting in 30 s\"; sleep 30; done" >> "$HOME/miner-$gpu.log" 2>&1 &
done
```

The log is `~/miner-0.log`. Choose hosts by CPU as much as by GPU: llama.cpp's
main thread is the bottleneck, so a recent desktop CPU (Ryzen 7000/9000, Intel
13th/14th gen, Core Ultra) serves far more than an old Xeon with the same card.

## 6. Measure

```sh
pool-miner bench --pool https://pool.example.com --models elephant-1-q8 --duration 120
```

`bench` runs fitting profiles (skipping published offload profiles when a full-GPU
profile of the model fits, unless selected via `--profile`/`--context`), without
joining the pool, with a fixed mix
of request sizes like the pool's (40% 300–1,500 tokens, 30% 1,500–4,000, 20%
4,000–12,000, 10% 12,000–40,000), and prints requests/s, questions/s, prompt
tokens/s, p50/p95 latency and peak GPU memory and RAM per profile. Add `--json`
for a machine-readable report. Stop a running miner on the same GPU first.

## Update and uninstall

Update: run the install command again, then restart the miner (or
`systemctl --user restart pool-miner`). Cached weights are kept.

Uninstall on Linux and macOS:

```sh
systemctl --user disable --now pool-miner 2>/dev/null
rm -f ~/.local/bin/pool-miner ~/.local/bin/pool-server ~/.config/systemd/user/pool-miner.service
rm -rf ~/.local/share/ai-pool ~/.cache/ai-pool        # macOS cache: ~/Library/Caches/ai-pool
```

Windows: delete `%LOCALAPPDATA%\ai-pool` (program and cache) and remove it from
your user PATH.

## Qwen conversational chat (0.2.8)

`qwen3.8-27b-ud-q3-k-xl` uses the existing `llama-b11374` runtime, commit
`b92761a515ea31e852e7fbc1fad5f874b46f3718`. Miners download the 13,146,393,504-byte
GGUF directly from `unsloth/Qwen3.8-27B-GGUF` at revision
`4ca720788d1e01f1bff70c033e0d0028fd02e502` and verify SHA256
`8c2a45ff85e7674ca185ec8eb6cdeab0e617ed9d8018caed0b64380eb2a67a5e` locally.
The pool distributes metadata and requests only; it never stores Qwen weights.

Select it with `--models qwen3.8-27b-ud-q3-k-xl --profile qwen3.8-27b-ud-q3-k-xl=cuda-8192`.
This provisional CUDA profile has 8,192 input-plus-output tokens, one slot,
512-token batches/microbatches, f16 KV cache, Jinja, no context shifting,
no vision/projector or draft/MTP, and no automatic RAM fallback. Its 14,586 MiB
memory estimate is not measured peak VRAM or a guarantee of fit. GPU memory,
template behavior, structured decoding and throughput await live qualification.
An unavailable miner returns an explicit error; no estimated token counts are used.

`POST /v1/chat/completions` accepts nested `response_format.json_schema.schema`,
`chat_template_kwargs` (including boolean `enable_thinking` and `preserve_thinking`),
and `temperature`, `top_p`, `top_k`, `min_p`, `presence_penalty`, `repeat_penalty`,
and `seed`. Native tools remain unsupported. `POST /v1/chat/completions/input_tokens`
accepts the same body and returns the exact formatted input count from the loaded
runtime with model revision, profile, context, fingerprint and instance metadata.
Count and completion share one normalized payload; schema/template options reach
both upstream endpoints unchanged.

`GET /v1/models/{id}` distinguishes catalog capacity from loaded availability and
reports pinned artifact/runtime expectations and available runtime descriptors.
Clients preflight a live descriptor, count with `pool.runtime_fingerprint`, then
complete with that fingerprint and `pool.max_attempts:1`. A missing or changed
fingerprint returns HTTP 409 `revision_mismatch`; identical configurations may
run on different instances. Instance IDs are diagnostic metadata. Completion
headers include model revision/profile, fingerprint, instance, request ID and
actual completion attempts. Provenance is miner-reported, not remote attestation;
launch placement is recorded but actual tensor placement is not measured.

New miners request `chat-json-v1,openjev-v1`. Older miners do not receive Elephant;
install v0.2.11 or newer to serve it. Qwen's artifact and runtime remain unchanged.
