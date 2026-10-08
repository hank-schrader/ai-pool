#!/usr/bin/env python3
"""Phase 0 runtime qualification: start a pinned llama-server with one context
profile, exercise it with a small and a near-limit request, and record peak
accelerator memory.

Usage:
  measure.py --server <llama-server> --model <gguf> --kind clef|chat --ctx 4096 [--ctx 8192 ...]

Prints one JSON line per profile. NVIDIA memory is sampled from nvidia-smi;
on other platforms the peak fields are null and must be read from the OS.
"""

import argparse
import json
import os
import shutil
import socket
import subprocess
import threading
import time
import urllib.error
import urllib.request


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def gpu_used_mib():
    if not shutil.which("nvidia-smi"):
        return None
    out = subprocess.run(
        ["nvidia-smi", "--query-gpu=memory.used", "--format=csv,noheader,nounits"],
        capture_output=True, text=True, check=True,
    ).stdout
    return int(out.splitlines()[0])


class PeakSampler(threading.Thread):
    def __init__(self):
        super().__init__(daemon=True)
        self.peak = None
        self.stop = threading.Event()

    def run(self):
        while not self.stop.is_set():
            used = gpu_used_mib()
            if used is not None and (self.peak is None or used > self.peak):
                self.peak = used
            time.sleep(0.1)


def post(url, body, timeout=600):
    request = urllib.request.Request(
        url, data=json.dumps(body).encode(), headers={"Content-Type": "application/json"}
    )
    started = time.monotonic()
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            return response.status, json.loads(response.read()), time.monotonic() - started
    except urllib.error.HTTPError as error:
        return error.code, json.loads(error.read() or b"{}"), time.monotonic() - started


def clef_body(words):
    return {
        "state": "Customer message: " + "please " * words + "I was charged twice.",
        "questions": {
            "route": {
                "type": "choice",
                "instructions": "Which team should handle this?",
                "criteria": {"billing": None, "shipping": None, "technical": None},
            },
            "angry": {"type": "noul", "instructions": "Is the customer angry?"},
            "urgency": {
                "type": "score",
                "instructions": "How urgent is this?",
                "criteria": ["can wait", "this week", "today", "right now"],
            },
        },
    }


def chat_body(words, max_tokens):
    return {
        "messages": [{"role": "user", "content": "please " * words + "Explain compute pools briefly."}],
        "max_tokens": max_tokens,
    }


def exercise(base, kind, ctx):
    """Returns (small, near_limit, over_limit) request summaries."""
    def summary(status, body, seconds):
        usage = body.get("usage", {})
        tokens = usage.get("input_tokens", usage.get("prompt_tokens"))
        return {"status": status, "input_tokens": tokens, "seconds": round(seconds, 3),
                "error": body.get("error", {}).get("message") if status != 200 else None}

    if kind == "clef":
        url = f"{base}/v1/systemone"
        status, body, seconds = post(url, clef_body(0))
        small = summary(status, body, seconds)
        overhead = small["input_tokens"] or 0
        # Each "please " is one token for this vocabulary; scale once from the measured count.
        words = int((ctx * 0.97) - overhead)
        status, body, seconds = post(url, clef_body(words))
        near = summary(status, body, seconds)
        if near["status"] == 200 and near["input_tokens"]:
            per_word = (near["input_tokens"] - overhead) / max(words, 1)
            words = int((ctx * 0.97 - overhead) / per_word)
            status, body, seconds = post(url, clef_body(words))
            near = summary(status, body, seconds)
        status, body, seconds = post(url, clef_body(ctx + 64))
        over = summary(status, body, seconds)
    else:
        url = f"{base}/v1/chat/completions"
        status, body, seconds = post(url, chat_body(0, 32))
        small = summary(status, body, seconds)
        overhead = small["input_tokens"] or 0
        max_tokens = 256
        words = int(ctx * 0.97 - overhead - max_tokens)
        status, body, seconds = post(url, chat_body(words, max_tokens))
        near = summary(status, body, seconds)
        status, body, seconds = post(url, chat_body(ctx + 64, 16))
        over = summary(status, body, seconds)
    return small, near, over


def measure(args, ctx):
    port = free_port()
    command = [
        args.server, "-m", args.model, "-ngl", "99", "-c", str(ctx),
        "--host", "127.0.0.1", "--port", str(port), "--parallel", "1",
    ]
    if args.kind == "clef":
        command += ["-b", str(ctx), "-ub", str(ctx)]
    else:
        command += ["-b", "512", "-ub", "128", "--no-context-shift"]
    baseline = gpu_used_mib()
    sampler = PeakSampler()
    sampler.start()
    log = open(f"{args.log_dir}/{args.kind}-{ctx}.log", "w") if args.log_dir else subprocess.DEVNULL
    child = subprocess.Popen(command, stdout=log, stderr=subprocess.STDOUT)
    base = f"http://127.0.0.1:{port}"
    started = time.monotonic()
    result = {"kind": args.kind, "model": os.path.basename(args.model), "ctx": ctx,
              "args": command[3:], "baseline_mib": baseline}
    try:
        while True:
            if child.poll() is not None:
                result["error"] = f"server exited with {child.returncode} during load"
                return result
            try:
                with urllib.request.urlopen(f"{base}/health", timeout=2) as response:
                    if response.status == 200:
                        break
            except (urllib.error.URLError, ConnectionError, TimeoutError):
                pass
            if time.monotonic() - started > 300:
                result["error"] = "health timeout"
                return result
            time.sleep(0.25)
        result["load_seconds"] = round(time.monotonic() - started, 1)
        time.sleep(1)
        result["loaded_mib"] = gpu_used_mib()
        result["small"], result["near_limit"], result["over_limit"] = exercise(base, args.kind, ctx)
    finally:
        child.terminate()
        try:
            child.wait(timeout=20)
        except subprocess.TimeoutExpired:
            child.kill()
            child.wait()
        sampler.stop.set()
        sampler.join()
        result["peak_mib"] = sampler.peak
        if sampler.peak is not None and baseline is not None:
            result["peak_delta_mib"] = sampler.peak - baseline
    return result


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--server", required=True)
    parser.add_argument("--model", required=True)
    parser.add_argument("--kind", choices=["clef", "chat"], required=True)
    parser.add_argument("--ctx", type=int, action="append", required=True)
    parser.add_argument("--log-dir")
    args = parser.parse_args()
    for ctx in args.ctx:
        print(json.dumps(measure(args, ctx)), flush=True)
        time.sleep(3)


if __name__ == "__main__":
    main()
