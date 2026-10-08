#!/usr/bin/env python3
"""Check that clef-token-count matches the pinned llama-server exactly.

Each case goes to both the helper and /v1/systemone. For accepted requests
the helper count must equal usage.input_tokens. For over-limit requests it
must equal the count in the server's "input (N tokens) is too large" error.
Invalid requests must be rejected by both.

Usage: compare_clef_counts.py --server <llama-server> --helper <clef-token-count> --model <gguf> [--ctx 16384]
"""

import argparse
import json
import re
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.request


def cases():
    route = {"type": "choice", "instructions": "Which team should handle this?",
             "criteria": {"billing": None, "shipping": None, "technical": None}}
    yield "string state, 3 question types", {
        "state": "Customer message: I was charged twice for my order last week and nobody has replied.",
        "questions": {
            "route": route,
            "angry": {"type": "noul", "instructions": "Is the customer angry?"},
            "urgency": {"type": "score", "instructions": "How urgent is this?",
                        "criteria": ["can wait", "this week", "today", "right now"]},
        },
    }
    yield "object state with nested arrays", {
        "state": {"ticket": {"id": 42, "tags": ["refund", "vip"], "body": "Refund please"}, "z": 1, "a": [1, 2.5, None, True]},
        "questions": {"refund": {"type": "noul", "instructions": "Is a refund requested?"}},
    }
    yield "array state", {
        "state": [{"role": "user", "content": "hello"}, {"role": "assistant", "content": "hi"}],
        "questions": {"greeting": {"type": "noul", "instructions": "Is this a greeting?"}},
    }
    yield "noul with criteria", {
        "state": "The build failed on main.",
        "questions": {"ci": {"type": "noul", "instructions": "Is CI broken?",
                             "criteria": {"true": "CI is failing", "false": "CI is fine"}}},
    }
    yield "choice with descriptions, unsorted keys", {
        "state": "My package never arrived.",
        "questions": {"route": {"type": "choice", "instructions": "Route it.",
                                "criteria": {"zeta": "Last option", "alpha": "First option", "mid": None}}},
    }
    yield "unicode and emoji", {
        "state": "Клиент пишет: «деньги списали дважды» 😡 — 日本語のテキストも。",
        "questions": {"lang": {"type": "choice", "instructions": "Main language?",
                               "criteria": {"ru": "Russian", "ja": "Japanese", "en": "English"}}},
    }
    yield "marker text inside input", {
        "state": "literal <<clef:sep>> and <<clef:question>> in user text",
        "questions": {"q": {"type": "noul", "instructions": "Does it mention <<clef:option>>?"}},
    }
    yield "object instructions", {
        "state": "x",
        "questions": {"q": {"type": "noul", "instructions": {"rule": "Answer yes if x", "examples": ["x", "y"]}}},
    }
    many = {f"q{i}": {"type": "noul", "instructions": f"Is statement {i} true?"} for i in range(16)}
    yield "16 questions", {"state": "Statements follow.", "questions": many}
    yield "64-option choice", {
        "state": "pick one",
        "questions": {"pick": {"type": "choice", "instructions": "Which?",
                               "criteria": {f"opt{i:02d}": f"Option number {i}" for i in range(64)}}},
    }
    yield "10-level score", {
        "state": "rate it",
        "questions": {"s": {"type": "score", "instructions": "Quality?", "criteria": [f"level {i}" for i in range(10)]}},
    }
    yield "long state near 4k", {
        "state": "word " * 3600, "questions": {"q": {"type": "noul", "instructions": "Long?"}},
    }
    yield "long state near 16k", {
        "state": "lorem ipsum dolor sit amet " * 3000, "questions": {"q": {"type": "noul", "instructions": "Long?"}},
    }
    yield "over limit", {
        "state": "please " * 20000, "questions": {"q": {"type": "noul", "instructions": "Long?"}},
    }
    yield "invalid: missing state", {"questions": {"q": {"type": "noul", "instructions": "x"}}}
    yield "invalid: bad type", {"state": "x", "questions": {"q": {"type": "maybe", "instructions": "x"}}}
    yield "invalid: empty instructions", {"state": "x", "questions": {"q": {"type": "noul", "instructions": ""}}}
    yield "invalid: one-option choice", {"state": "x", "questions": {"q": {"type": "choice", "instructions": "x", "criteria": {"a": None}}}}


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def post(url, body):
    request = urllib.request.Request(url, data=json.dumps(body).encode(), headers={"Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(request, timeout=600) as response:
            return response.status, json.loads(response.read())
    except urllib.error.HTTPError as error:
        return error.code, json.loads(error.read() or b"{}")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--server", required=True)
    parser.add_argument("--helper", required=True)
    parser.add_argument("--model", required=True)
    parser.add_argument("--ctx", type=int, default=16384)
    args = parser.parse_args()

    helper = subprocess.Popen([args.helper, args.model], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                              stderr=subprocess.DEVNULL, text=True)
    ready = json.loads(helper.stdout.readline())
    assert ready.get("ready"), ready

    port = free_port()
    server = subprocess.Popen([args.server, "-m", args.model, "-ngl", "99", "-c", str(args.ctx), "-b", str(args.ctx),
                               "-ub", str(args.ctx), "--parallel", "1", "--host", "127.0.0.1", "--port", str(port)],
                              stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    base = f"http://127.0.0.1:{port}"
    for _ in range(600):
        try:
            with urllib.request.urlopen(f"{base}/health", timeout=2) as r:
                if r.status == 200:
                    break
        except (urllib.error.URLError, ConnectionError):
            time.sleep(0.25)

    failures = 0
    try:
        for n, (name, body) in enumerate(cases()):
            started = time.monotonic()
            helper.stdin.write(json.dumps({"id": n, "body": body}) + "\n")
            helper.stdin.flush()
            counted = json.loads(helper.stdout.readline())
            helper_ms = (time.monotonic() - started) * 1000
            status, reply = post(f"{base}/v1/systemone", body)
            if status == 200:
                expected = reply["usage"]["input_tokens"]
            else:
                message = reply.get("error", {}).get("message", "")
                match = re.search(r"input \((\d+) tokens\) is too large", message)
                expected = int(match.group(1)) if match else None
            got = counted.get("input_tokens")
            if expected is None:
                ok = got is None and "error" in counted
                detail = f"server {status}: {reply.get('error', {}).get('message', '')[:60]!r}; helper: {counted.get('error')}"
            else:
                ok = got == expected
                detail = f"server {status} {expected} tokens; helper {got} ({helper_ms:.1f} ms)"
            failures += not ok
            print(f"{'PASS' if ok else 'FAIL'}  {name}: {detail}")
    finally:
        server.terminate()
        server.wait()
        helper.stdin.close()
        helper.wait()
    print(f"{failures} failure(s)")
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
