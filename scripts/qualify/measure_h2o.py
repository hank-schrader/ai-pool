#!/usr/bin/env python3
"""Exercise H2O with the pool's pinned launch flags and label-only readout.

Each run checks labels, a known answer, full-context requests on every slot,
and an over-limit rejection. Collect DRM fdinfo where the kernel exposes it;
on Asahi, --memory-trace uses the companion GEM allocation tracer. Summing
GPU allocations and RSS is conservative if mapped pages appear in both.
"""

import argparse
import concurrent.futures
import glob
import json
import os
import re
import subprocess
import threading
import time

from measure import buffer_sizes, free_port, post, rss_mib

SYSTEM = ("You are a decision engine. You read a record and answer one question about it by choosing "
          "exactly one option. Reply with a single letter.")


def render(state):
    user = (f"record: {state}\nquestion: Is the sky blue?\noptions:\n"
            "A) true: the statement holds\nB) false: it does not")
    return (f"<|im_start|>system\n{SYSTEM}<|im_end|>\n<|im_start|>user\n{user}<|im_end|>\n"
            "<|im_start|>assistant\n<think>\n\n</think>\n\nAnswer:")


def drm_memory(pid):
    clients = {}
    for path in glob.glob(f"/proc/{pid}/fdinfo/*"):
        try:
            fields = dict(line.strip().split(":", 1) for line in open(path) if ":" in line)
            client = fields.get("drm-client-id")
            if client is not None:
                values = {k: int(v.split()[0]) for k, v in fields.items()
                          if k.startswith(("drm-total-", "drm-resident-"))}
                clients[client] = values
        except (OSError, ValueError):
            pass
    return {key: sum(values.get(key, 0) for values in clients.values()) // 1024
            for key in {key for values in clients.values() for key in values}}


def measure(args):
    port = free_port()
    base = f"http://127.0.0.1:{port}"
    command = [args.server, "-m", args.model, "-ngl", str(args.ngl),
               "-c", str((args.ctx + 2) * args.slots), "--parallel", str(args.slots),
               "-b", str(args.batch), "-ub", str(args.batch), "-fa", "on", "-ctk", "f16", "-ctv", "f16",
               "--no-context-shift", "--cache-ram", "0", "--ctx-checkpoints", "0",
               "--host", "127.0.0.1", "--port", str(port), "-v"]
    if args.cpu_only:
        command += ["--device", "none", "--no-op-offload"]
    if args.no_mmap:
        command += ["--load-mode", "none"]
    if args.threads:
        command += ["-t", str(args.threads), "-tb", str(args.threads)]
    def available():
        fields = dict(line.split(":", 1) for line in open("/proc/meminfo"))
        return int(fields["MemAvailable"].split()[0]) // 1024
    baseline_available = available()
    peaks = {"rss_mib": 0, "system_memory_delta_mib": 0}
    stop = threading.Event()
    started = time.monotonic()
    result = {"ctx": args.ctx, "slots": args.slots, "ngl": args.ngl, "baseline_available_mib": baseline_available, "command": command}
    with open(args.log, "w") as log:
        env = os.environ.copy()
        if args.memory_trace:
            env["LD_PRELOAD"] = args.memory_trace
        child = subprocess.Popen(command, stdout=log, stderr=subprocess.STDOUT, env=env)
        def sample():
            while not stop.is_set():
                values = {"rss_mib": rss_mib(child.pid) or 0, "system_memory_delta_mib": max(0, baseline_available - available()), **drm_memory(child.pid)}
                for key, value in values.items():
                    peaks[key] = max(peaks.get(key, 0), value)
                stop.wait(0.05)
        sampler = threading.Thread(target=sample, daemon=True)
        sampler.start()
        try:
            while True:
                if child.poll() is not None:
                    raise RuntimeError(f"server exited: {child.returncode}; see {args.log}")
                try:
                    import urllib.request
                    urllib.request.urlopen(f"{base}/health", timeout=1).close()
                    break
                except (OSError, TimeoutError):
                    if time.monotonic() - started > 300:
                        raise RuntimeError("health timeout")
                    time.sleep(0.25)
            result["load_seconds"] = round(time.monotonic() - started, 3)
            def tokenize(prompt):
                status, body, _ = post(f"{base}/tokenize", {"content": prompt, "add_special": False, "parse_special": True})
                assert status == 200, body
                return body["tokens"]
            prompt = render("The sky is blue.")
            tokens = tokenize(prompt)
            labels = []
            for label in "AB":
                full = tokenize(prompt + " " + label)
                assert full[:-1] == tokens and full[-1] not in labels
                labels.append(full[-1])
            def ask(tokens, slot=0):
                status, body, seconds = post(f"{base}/completion", {
                    "prompt": tokens, "id_slot": slot, "logit_bias": [[label, 64.0] for label in labels],
                    "n_predict": 1, "n_probs": 2, "samplers": [], "post_sampling_probs": True,
                    "cache_prompt": True, "stream": False, "temperature": 1.0,
                    "backend_sampling": False, "mirostat": 0, "seed": 0,
                })
                output = {"status": status, "seconds": round(seconds, 3), "input_tokens": len(tokens),
                          "timings": body.get("timings"), "truncated": body.get("truncated"),
                          "tokens_evaluated": body.get("tokens_evaluated"),
                          "probabilities": body.get("completion_probabilities"), "error": body.get("error")}
                return output
            result["small"] = ask(tokens)
            assert result["small"]["status"] == 200, result["small"]
            probs = {p["id"]: p["prob"] for p in result["small"]["probabilities"][0]["top_probs"]}
            assert probs[labels[0]] > probs[labels[1]], result["small"]
            full = tokenize(render("please " * (args.ctx - len(tokenize(render(""))))))
            assert len(full) == args.ctx, len(full)
            with concurrent.futures.ThreadPoolExecutor(args.slots) as workers:
                result["full_context"] = list(workers.map(lambda slot: ask(full, slot), range(args.slots)))
            for reply in result["full_context"]:
                assert reply["status"] == 200 and reply["tokens_evaluated"] == args.ctx and reply["truncated"] is False, reply
            result["over_limit"] = ask(full + full[-1024:])
            assert result["over_limit"]["status"] == 400, result["over_limit"]
        except Exception as error:
            result["error"] = str(error)
        finally:
            child.terminate()
            try:
                child.wait(timeout=20)
            except subprocess.TimeoutExpired:
                child.kill()
                child.wait()
            stop.set()
            sampler.join()
    trace = re.search(r"asahi_gem_peak_bytes=(\d+) asahi_gem_tracking_overflow=0", open(args.log).read())
    if args.memory_trace and not trace:
        result["error"] = "Asahi allocation trace missing or tracking overflowed"
    if trace:
        peaks["asahi_gem_mib"] = (int(trace[1]) + (1 << 20) - 1) // (1 << 20)
        peaks["conservative_unified_mib"] = peaks["asahi_gem_mib"] + peaks["rss_mib"]
    result["peaks"] = peaks
    result["buffers_mib"] = buffer_sizes(args.log)
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--server", required=True)
    parser.add_argument("--model", required=True)
    parser.add_argument("--ctx", type=int, required=True)
    parser.add_argument("--slots", type=int, default=1)
    parser.add_argument("--ngl", type=int, default=99)
    parser.add_argument("--cpu-only", action="store_true")
    parser.add_argument("--no-mmap", action="store_true")
    parser.add_argument("--threads", type=int)
    parser.add_argument("--batch", type=int, default=2048)
    parser.add_argument("--memory-trace")
    parser.add_argument("--log", required=True)
    print(json.dumps(measure(parser.parse_args())), flush=True)


if __name__ == "__main__":
    main()
