#!/usr/bin/env python3
"""Fixed-size H2O requests through a local pool, with runtime prompt timings.

pool-miner bench has a seeded size mix rather than a fixed prompt-size option.
This harness uses the real pool/miner H2O engine and the published CPU profiles.
All payloads have one realistic billing question. Different
case identifiers prevent reusing an entire cached prompt. Generation is measured
separately by llama-bench tg32 after prefill at the requested depth; H2O itself
only reads one label and cannot supply a sustained generation rate.
"""

import argparse
import copy
import datetime
import glob
import json
import os
from pathlib import Path
import re
import shlex
import signal
import socket
import statistics
import subprocess
import time
import urllib.request

SYSTEM = ("You are a decision engine. You read a record and answer one question about it by choosing "
          "exactly one option. Reply with a single letter.")
QUESTION = {
    "type": "choice",
    "instructions": "Which team should resolve the duplicate charge described in the record?",
    "criteria": {"billing": None, "shipping": None, "technical": None},
}
HISTORY = [
    "The customer ordered a replacement part through the online store. The order confirmation listed one item and one delivery fee.",
    "The warehouse confirmed that the part was in stock. The parcel was packed and handed to the carrier on the next working day.",
    "Tracking showed the parcel moving through the regional depot. The customer received the correct item and confirmed that it was undamaged.",
    "The payment history lists two completed card charges for the same order. Both charges have settled and neither is a temporary authorization.",
    "Support checked the order number and the receipt. The customer supplied a bank statement showing two equal payments with different transaction identifiers.",
    "The customer asked for one duplicate payment to be refunded. No replacement shipment is needed, and the account can be accessed normally.",
    "An agent noted that the item price and delivery fee were correct. The outstanding issue is the extra settled payment, not the delivery or the software.",
]
TAIL = "\nCurrent issue: the order was delivered correctly, but the customer was charged twice and requests a refund of the duplicate payment."
PROMPT_TIMING = re.compile(r"prompt eval time\s*=\s*([\d.]+) ms /\s*(\d+) tokens")
MODES = ("cpu-final", "vulkan", "cpu", "ram-vulkan")


def free_port():
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return listener.getsockname()[1]


def call(base, route, body=None, key=None, timeout=180):
    headers = {"Content-Type": "application/json"}
    if key:
        headers["Authorization"] = "Bearer " + key
    request = urllib.request.Request(base + route, data=None if body is None else json.dumps(body).encode(), headers=headers)
    with urllib.request.urlopen(request, timeout=timeout) as response:
        return response.status, dict(response.headers), json.load(response)


def wait(predicate, seconds):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        try:
            value = predicate()
            if value:
                return value
        except (OSError, ValueError, KeyError):
            pass
        time.sleep(0.1)
    raise TimeoutError("condition did not become ready")


def stop(child, interrupt=False):
    if child is None or child.poll() is not None:
        return
    child.send_signal(signal.SIGINT if interrupt else signal.SIGTERM)
    try:
        child.wait(timeout=40)
    except subprocess.TimeoutExpired:
        child.kill()
        child.wait(timeout=10)


def runtime_child(miner):
    for status_path in glob.glob("/proc/[0-9]*/status"):
        try:
            fields = dict(line.split(":", 1) for line in open(status_path) if ":" in line)
            if int(fields["PPid"]) != miner.pid or fields["Name"].strip() != "llama-server":
                continue
            pid = int(status_path.split("/")[2])
            argv = Path(f"/proc/{pid}/cmdline").read_bytes().decode().split("\0")
            # Keep the private runtime key in memory only; never write it to results.
            return pid, "http://127.0.0.1:" + argv[argv.index("--port") + 1], argv[argv.index("--api-key") + 1]
        except (OSError, ValueError):
            pass
    return None


def render(state):
    options = "\n".join(f"{label}) {name}: {name}" for label, name in zip("ABC", QUESTION["criteria"]))
    user = f"record: {state}\nquestion: {QUESTION['instructions']}\noptions:\n{options}"
    return (f"<|im_start|>system\n{SYSTEM}<|im_end|>\n<|im_start|>user\n{user.strip()}<|im_end|>\n"
            "<|im_start|>assistant\n<think>\n\n</think>\n\nAnswer:")


def payload_at_size(base, key, target, case):
    def tokenize(content):
        return call(base, "/tokenize", {"content": content, "add_special": False, "parse_special": True}, key)[2]["tokens"]
    prefix = f"Customer support case {case:08d}. The customer reports a duplicate settled payment.\n"
    corpus = "\n".join(f"History entry {i + 1}: {HISTORY[i % len(HISTORY)]}" for i in range(target // 8))
    low, high = 0, len(corpus)
    while high - low > 1:
        mid = (low + high) // 2
        if len(tokenize(render(prefix + corpus[:mid].rstrip() + TAIL))) <= target:
            low = mid
        else:
            high = mid
    for length in range(max(0, low - 64), min(len(corpus), low + 65)):
        state = prefix + corpus[:length].rstrip() + TAIL
        count = len(tokenize(render(state)))
        if count == target:
            return {"model": "h2o-q8", "state": state, "questions": {"route": QUESTION}}, len(tokenize(state))
    raise RuntimeError(f"could not construct an exact {target}-token prompt")


def measure_pool(args, mode):
    original = json.loads(args.catalog.read_text())
    model = copy.deepcopy(next(model for model in original["models"] if model["id"] == "h2o-q8"))
    if mode == "cpu-final":
        profile = next(profile for profile in model["profiles"]
                       if profile["accelerator"] == "cpu" and profile["context_tokens"] == args.context)
    else:
        profile = {"id": f"measure-{mode}-{args.context}", "accelerator": "cpu", "context_tokens": args.context,
                   "memory_mib": 6144, "memory_source": "estimate"}
    model["profiles"] = [profile]
    original["models"] = [model]
    original["default_models"] = ["h2o-q8"]
    catalog = args.output / f"catalog-{mode}.json"
    catalog.write_text(json.dumps(original, indent=2) + "\n")
    wrapper = args.output / f"server-{mode}.sh"
    # cpu-final uses the miner's own batch, topology and loading defaults.
    # Other modes reproduce the historical comparison through this CPU miner.
    extra = []
    if mode != "cpu-final":
        extra = ["--threads", str(args.threads), "--threads-batch", str(args.threads),
                 "-b", "2048", "-ub", "2048", "--load-mode", "auto",
                 "-ngl", "99" if mode == "vulkan" else "0",
                 "--device", "none" if mode == "cpu" else "Vulkan0",
                 "--no-op-offload" if mode == "cpu" else "--op-offload"]
    wrapper.write_text("#!/bin/sh\nexec " + shlex.quote(str(args.server)) + ' "$@" ' + shlex.join(extra) + "\n")
    wrapper.chmod(0o755)
    base = f"http://127.0.0.1:{free_port()}"
    env = os.environ.copy()
    env.update(POOL_BIND=base.removeprefix("http://"), POOL_AUTH_MODE="local", POOL_CATALOG=str(catalog),
               POOL_REQUEST_TIMEOUT_SECONDS="600", POOL_QUEUE_TIMEOUT_SECONDS="300", RUST_LOG="info,llama_server=debug")
    miner = pool = None
    pool_log = args.output / f"pool-{mode}.log"
    miner_log = args.output / f"miner-{mode}.log"
    rows = []
    try:
        with pool_log.open("w") as log:
            pool = subprocess.Popen([str(args.pool_server)], env=env, cwd=args.output, stdout=log, stderr=subprocess.STDOUT)
        wait(lambda: call(base, "/readyz"), 20)
        with miner_log.open("w") as log:
            miner = subprocess.Popen([str(args.miner), "--pool", base, "--models", "h2o-q8", "--yes",
                                      "--profile", "h2o-q8=" + profile["id"], "--llama-server", str(wrapper),
                                      "--prefetch", "0", "--allow-unqualified-runtime"],
                                     env=env, cwd=args.output, stdout=log, stderr=subprocess.STDOUT)
        wait(lambda: call(base, "/v1/models")[2]["data"][0]["pool"]["ready_miners"] == 1, 180)
        pid, runtime, key = wait(lambda: runtime_child(miner), 5)
        argv = Path(f"/proc/{pid}/cmdline").read_bytes().decode().split("\0")
        def value(flag):
            return next(argv[i + 1] for i in reversed(range(len(argv) - 1)) if argv[i] == flag)
        assert value("-ngl") == ("99" if mode == "vulkan" else "0")
        if mode == "cpu-final":
            assert value("--load-mode") == "none"
            assert value("-b") == "512"
            assert value("-t") == str(args.threads)
        assert value("--parallel") == "1"
        print(json.dumps({"stage": "pool_ready", "mode": mode, "slots": 1}), flush=True)
        for size in args.sizes:
            bodies = [payload_at_size(runtime, key, size, 10000 * size + i) for i in range(args.requests)]
            offset = len(miner_log.read_bytes())
            samples = []
            started = time.monotonic()
            for body, state_tokens in bodies:
                tick = time.monotonic()
                status, headers, answer = call(base, "/v1/systemone", body, timeout=180)
                elapsed = time.monotonic() - tick
                assert status == 200 and answer["usage"]["input_tokens"] == size, answer
                probs = answer["answers"]["route"]["probabilities"]
                assert max(probs, key=probs.get) == "billing", answer
                samples.append({"seconds": elapsed, "state_tokens": state_tokens, "usage": answer["usage"],
                                "answer": answer["answers"], "profile": headers.get("x-model-profile")})
            wall = time.monotonic() - started
            def timings():
                found = PROMPT_TIMING.findall(miner_log.read_bytes()[offset:].decode())
                return found if len(found) == args.requests else None
            measured = wait(timings, 3)
            for sample, (ms, tokens) in zip(samples, measured):
                sample["prompt_ms"] = float(ms)
                sample["computed_prompt_tokens"] = int(tokens)
                assert int(tokens) == size, f"unexpected prompt cache reuse: {sample}"
            row = {"mode": mode, "prompt_tokens": size, "context_limit": args.context, "parallel_slots": 1,
                   "requests": args.requests, "median_latency_seconds": statistics.median(s["seconds"] for s in samples),
                   "prompt_tokens_per_second": sum(s["computed_prompt_tokens"] for s in samples) /
                       (sum(s["prompt_ms"] for s in samples) / 1000),
                   "questions_per_second": args.requests / wall, "wall_seconds": wall, "samples": samples}
            rows.append(row)
            (args.output / f"requests-{mode}.json").write_text(json.dumps(rows, indent=2) + "\n")
            print(json.dumps({"stage": "requests", **{k: v for k, v in row.items() if k != "samples"}}), flush=True)
    finally:
        stop(miner, interrupt=True)
        stop(pool)
    return rows


def measure_generation(args, mode):
    batch = "512" if mode == "cpu-final" else "2048"
    command = [str(args.bench), "-m", str(args.weights), "-ngl", "99" if mode == "vulkan" else "0",
               "-dev", "none" if mode in ("cpu", "cpu-final") else "Vulkan0", "-p", "0", "-n", "32", "-d",
               ",".join(str(size) for size in args.sizes), "-b", batch, "-ub", batch, "-fa", "on",
               "-ctk", "f16", "-ctv", "f16", "-t", str(args.threads), "-r", "3", "--progress", "-o", "json"]
    if mode in ("cpu", "cpu-final"):
        command += ["-nopo", "1"]
    if mode == "cpu-final":
        command += ["--load-mode", "none"]
    path = args.output / f"generation-{mode}.json"
    if not args.reuse_generation:
        with path.open("w") as output, (args.output / f"generation-{mode}.log").open("w") as log:
            child = subprocess.Popen(command, stdout=output, stderr=log)
            try:
                if child.wait(timeout=600) != 0:
                    raise RuntimeError(f"llama-bench failed: {mode}")
            finally:
                stop(child)
    reports = json.loads(path.read_text())
    assert len(reports) == len(args.sizes)
    for report in reports:
        assert report["build_commit"] == "b92761a51" and report["n_gen"] == 32 and report["n_prompt"] == 0
        assert report["n_gpu_layers"] == (99 if mode == "vulkan" else 0)
        assert report["no_op_offload"] == (mode in ("cpu", "cpu-final"))
    return {report["n_depth"]: report for report in reports}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--catalog", type=Path, required=True)
    parser.add_argument("--server", type=Path, required=True)
    parser.add_argument("--bench", type=Path, required=True)
    parser.add_argument("--miner", type=Path, required=True)
    parser.add_argument("--pool-server", type=Path, required=True)
    parser.add_argument("--weights", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--sizes", nargs="+", type=int, default=[800, 2048, 4096])
    parser.add_argument("--modes", nargs="+", choices=MODES, default=["cpu-final"])
    parser.add_argument("--threads", type=int, default=6)
    parser.add_argument("--context", type=int, default=8192)
    parser.add_argument("--requests", type=int, default=3)
    parser.add_argument("--reuse-generation", action="store_true")
    parser.add_argument("--wait-for-pid", type=int)
    args = parser.parse_args()
    if args.requests < 1 or args.threads < 1 or any(size < 512 or size > args.context for size in args.sizes):
        parser.error("use positive requests/threads and prompt sizes between 512 and the context limit")
    for key in ["catalog", "server", "bench", "miner", "pool_server", "weights", "output"]:
        setattr(args, key, getattr(args, key).resolve())
    args.output.mkdir(parents=True, exist_ok=True)
    if args.wait_for_pid:
        wait(lambda: not Path(f"/proc/{args.wait_for_pid}").exists(), 900)
    rows = []
    result = {"schema_version": 1, "date": datetime.datetime.now(datetime.timezone.utc).isoformat(),
              "uname": list(os.uname()), "load_average_before": list(os.getloadavg()), "threads": args.threads,
              "method": "local pool/miner H2O systemone and llama-server prompt timers; llama-bench tg32 at matching depth",
              "slots": 1, "requests_per_size": args.requests, "rows": rows}
    destination = args.output / "results.json"
    try:
        for mode in args.modes:
            generation = measure_generation(args, mode)
            for row in measure_pool(args, mode):
                report = generation[row["prompt_tokens"]]
                row["generation_tokens_per_second"] = report["avg_ts"]
                row["generation_stddev_tokens_per_second"] = report["stddev_ts"]
                row["generation_samples_ns"] = report["samples_ns"]
                rows.append(row)
                destination.write_text(json.dumps(result, indent=2) + "\n")
    except Exception as error:
        result["error"] = repr(error)
        raise
    finally:
        destination.write_text(json.dumps(result, indent=2) + "\n")


if __name__ == "__main__":
    main()
