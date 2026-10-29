# ai-pool miner

A miner serves ai-pool's models from your GPU. It connects out to the pool over
HTTPS, so it needs no open ports. The examples use `https://pool.example.com`;
replace it with the address of the pool you join. The pool serves one model,
`h2o-q8` (Q8_0, 4.48 GB). It is also the default. The former BF16 model id
`h2o` has been removed; clients using it receive `model_not_found`.

## Requirements

| | Linux (x86_64) | Linux (aarch64) | Windows (x86_64) |
|---|---|---|---|
| Accelerator | NVIDIA, driver 570.26 or newer | ARM CPU; Apple Silicon under Asahi Linux tested | NVIDIA, driver 615 or newer (not tested yet) |
| OS | glibc 2.38+: Ubuntu 24.04+, Debian 13+, Fedora 39+, Arch | Fedora Asahi Remix 44 tested, including its 16K-page kernel; glibc 2.38+ | Windows 10/11 |
| Libraries | `libgomp`, OpenSSL 3, `libstdc++` | same; no GPU driver required | included |

Apple Silicon Macs can mine under Asahi Linux. macOS cannot mine `h2o-q8`: it has no Metal profile.

CUDA memory budgets for `h2o-q8` (catalog estimates). The miner subtracts
256 MiB from free GPU memory and 2 GiB from available RAM before planning:

| GPU budget | RAM budget | What the miner runs |
|---|---|---|
| 13,312 MiB | | `cuda-40960-x6`: 6 requests at once, prompts up to 40,960 tokens |
| 10,240 MiB | | `cuda-40960-x4`: 4 at once |
| 9,216 MiB | | `cuda-40960-x3`: 3 at once |
| 5,120 MiB | | `cuda-8192`: 1 at once, prompts up to 8,192 tokens |
| 1,280 MiB | 7,168 MiB | `cuda-8192` with the weights in RAM (automatic, much slower) |

On ARM Linux, `cpu-8192` runs 8,192-token prompts with one slot;
`cpu-4096` provides a smaller context when the startup probe needs to step down.
Both reserve **6,144 MiB of system RAM**, including measured headroom on the
Asahi M1 Pro. The miner budgets Linux's available RAM minus 2 GiB for the OS.
Other ARM machines use the same profiles and must pass their own startup probe.

The CPU backend keeps weights in RAM without mmap (`--load-mode none`), avoiding
a duplicate mapped copy when weights are repacked. It uses 512-token batches
and the performance-core count on heterogeneous CPUs, or all available cores on
uniform machines. This answered real H2O requests faster than Vulkan on the test
laptop with a comparable memory footprint. See [the Asahi qualification](docs/qualification/linux-asahi.md)
for memory, speed and cancellation measurements.

Disk: about 7 GB for the weights (4.48 GB) and the llama.cpp runtime.

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
files alone (`curl … | sh -s -- --version v0.2.6`).

Windows (PowerShell):

```powershell
irm https://raw.githubusercontent.com/hank-schrader/ai-pool/main/scripts/install.ps1 | iex
# open a new terminal
pool-miner --version
```

It installs into `%LOCALAPPDATA%\ai-pool\<version>` and adds it to your user
PATH. Pin a release with `.\install.ps1 -Version v0.2.6`.

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
pool-miner --pool https://pool.example.com --models h2o-q8
```

The miner picks the largest profile that fits, asks before downloading the
4.48 GB of weights, verifies their sha256, starts llama.cpp, answers a
full-length test prompt, and connects:

```
Profile plan:
  h2o-q8   cuda-40960-x4   context 40960   10240 MiB (estimate), 4 cache slots
...
h2o-q8: cuda-40960-x4 passed its 40960-token probe in …s
connected to https://pool.example.com (session ses_…)
```

On a small GPU the plan says `automatic RAM fallback (-ngl 0)` instead, and the
model serves prompts up to 8,192 tokens with its weights in system RAM.

- Windows: add `--allow-unqualified-runtime`, or answer `y` when asked; it
  has not been tested on hardware. The ARM Linux CPU runtime is qualified on
  the Asahi test machine and does not need this flag.
- `--yes` skips the download question (needed without a terminal).
- `--context h2o-q8=8192` or `--profile h2o-q8=cuda-40960-x3` forces a profile;
  `--device 1` picks the second GPU. Run one miner per GPU.
- Weights you already have can be imported instead of downloaded:
  `pool-miner download h2o-q8 --import /path/to/h2o-lightning-4b-672dc01e.Q8_0.gguf`.
- Ctrl-C finishes the jobs in progress and stops; press it twice to stop at once.
  The miner reconnects by itself when the pool restarts.

## 4. Keep it running

Linux, as a systemd user service:

```sh
mkdir -p ~/.config/systemd/user
cat > ~/.config/systemd/user/pool-miner.service <<'EOF'
[Unit]
Description=ai-pool h2o-q8 miner
After=network-online.target

[Service]
ExecStart=%h/.local/bin/pool-miner --pool https://pool.example.com --models h2o-q8 --yes
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
schtasks /Create /SC ONLOGON /TN ai-pool-miner /TR "pool-miner --pool https://pool.example.com --models h2o-q8 --yes --allow-unqualified-runtime"
```

## 5. Rented GPUs (Vast.ai and similar)

Use an Ubuntu 24.04 based CUDA image, for example
`vastai/base-image:cuda-12.8.1-auto`, SSH launch mode, at least 40 GB of disk,
and this on-start script. It runs on every start, including after a stop/start:

```bash
#!/bin/bash
POOL=https://pool.example.com
MODELS=h2o-q8
VERSION=v0.2.6     # pool-miner release to install

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
pool-miner bench --pool https://pool.example.com --models h2o-q8 --duration 120
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
