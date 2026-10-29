#!/usr/bin/env python3
"""Measure llama-server disconnect cancellation between H2O decode batches.

Observe the log without polling /slots: b11374 restarts its disconnect-check
wait whenever another task produces a result. Frequent /slots polling can
therefore suppress the cancellation that the measurement is trying to observe.
"""
import argparse
import http.client
import json
from pathlib import Path
import subprocess
import time

from measure import free_port, post


def measure(args, batch):
    port = free_port()
    base = f"http://127.0.0.1:{port}"
    log_path = args.output / f"cancel-{batch}.log"
    command = [str(args.server), "-m", str(args.model), "-ngl", "0", "--device", "none",
               "--no-op-offload", "--load-mode", "none", "-t", str(args.threads), "-tb", str(args.threads),
               "-c", "8194", "--parallel", "1", "-b", str(batch), "-ub", str(batch),
               "-fa", "on", "-ctk", "f16", "-ctv", "f16", "--no-context-shift", "--cache-ram", "0",
               "--ctx-checkpoints", "0", "--host", "127.0.0.1", "--port", str(port), "-v"]
    def wait(predicate, timeout):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if child.poll() is not None:
                raise RuntimeError(f"runtime exited: {child.returncode}")
            try:
                value = predicate()
                if value:
                    return value
            except OSError:
                pass
            time.sleep(.025)
        raise TimeoutError("condition did not become ready")
    with log_path.open("w") as log:
        child = subprocess.Popen(command, stdout=log, stderr=subprocess.STDOUT)
        connection = None
        try:
            import urllib.request
            wait(lambda: urllib.request.urlopen(base + "/health", timeout=1).close() or True, 300)
            status, reply, _ = post(base + "/tokenize", {"content": "please " * 6000, "add_special": False})
            assert status == 200
            offset = len(log_path.read_text())
            connection = http.client.HTTPConnection("127.0.0.1", port, timeout=120)
            connection.request("POST", "/completion", json.dumps({
                "prompt": reply["tokens"], "n_predict": 1, "cache_prompt": False, "stream": False,
            }), {"Content-Type": "application/json"})
            wait(lambda: "new prompt" in log_path.read_text()[offset:], 10)
            time.sleep(1)
            started = time.monotonic()
            connection.close()
            wait(lambda: "cancel task" in log_path.read_text()[offset:], 10)
            posted = time.monotonic() - started
            wait(lambda: "stop processing:" in log_path.read_text()[offset:], 45)
            released = time.monotonic() - started
            slots = json.load(urllib.request.urlopen(base + "/slots", timeout=10))
            assert all(not slot["is_processing"] for slot in slots)
            excerpt = "\n".join(line for line in log_path.read_text()[offset:].splitlines()
                                if any(part in line for part in ["cancel task", "stop processing:", "prompt processing progress"]))
            return {"batch": batch, "threads": args.threads, "prompt_tokens": len(reply["tokens"]),
                    "cancel_posted_seconds": posted, "slot_released_seconds": released, "runtime_excerpt": excerpt}
        finally:
            if connection:
                connection.close()
            child.terminate()
            try:
                child.wait(timeout=20)
            except subprocess.TimeoutExpired:
                child.kill()
                child.wait()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--server", type=Path, required=True)
    parser.add_argument("--model", type=Path, required=True)
    parser.add_argument("--threads", type=int, default=6)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    results = []
    for batch in [2048, 512]:
        result = measure(args, batch)
        print(json.dumps(result), flush=True)
        results.append(result)
        (args.output / "results.json").write_text(json.dumps(results, indent=2) + "\n")


if __name__ == "__main__":
    main()
