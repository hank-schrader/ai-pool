#!/usr/bin/env python3
"""One refactor-tool H2O question, reproduced from crates/llm-connector:
server flags from engine.rs (context 40960, 3 slots, -b 2048 -ub 512, -fa on,
f16 KV, --cache-ram 1024, --ctx-checkpoints 8), prompt from lightning_prompt.rs,
/tokenize + /completion bodies from lightning.rs / lightning_transport.rs.
Only -ngl differs (argv), because the weights do not fit a 4 GB GPU."""
import json, os, subprocess, sys, threading, time, urllib.request

server, model, record_path, ngl, ub = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4], sys.argv[5]
CTX, SLOTS, PORT, VOCAB = 40960, 3, "18990", 248_320
SYSTEM = ("You are a decision engine. You read a record and answer one question about it by choosing "
          "exactly one option. Reply with a single letter.")
LABELS = "ABCDEFGHIJKLMNOP"

def render(state, instructions, options):
    lines = "\n".join(f"{LABELS[i]}) {name}: {desc}" for i, (name, desc) in enumerate(options))
    user = f"record: {state}\nquestion: {instructions.lstrip()}\noptions:\n{lines}"
    return (f"<|im_start|>system\n{SYSTEM}<|im_end|>\n<|im_start|>user\n{user.strip()}<|im_end|>\n"
            f"<|im_start|>assistant\n<think>\n\n</think>\n\nAnswer:")

def post(route, body, timeout=900):
    data = body if isinstance(body, bytes) else json.dumps(body).encode()
    request = urllib.request.Request(f"http://127.0.0.1:{PORT}{route}", data=data, headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(request, timeout=timeout) as response:
        return json.loads(response.read())

def tokenize(prompt):
    return post("/tokenize", {"content": prompt, "add_special": False, "parse_special": True})["tokens"]

def gpu_used():
    out = subprocess.run(["nvidia-smi", "--query-gpu=memory.used", "--format=csv,noheader,nounits"], capture_output=True, text=True).stdout
    return int(out.split()[0])

def rss_mib(pid):
    for line in open(f"/proc/{pid}/status"):
        if line.startswith("VmRSS:"):
            return int(line.split()[1]) // 1024

baseline = gpu_used()
args = [server, "-m", model, "-ngl", ngl, "-c", str((CTX + 2) * SLOTS), "-np", str(SLOTS), "-b", "2048", "-ub", ub,
        "-fa", "on", "-ctk", "f16", "-ctv", "f16", "--no-context-shift", "--cache-ram", "1024", "--ctx-checkpoints", "8",
        "--host", "127.0.0.1", "--port", PORT]
started = time.time()
child = subprocess.Popen(args, stdout=subprocess.DEVNULL, stderr=open(os.path.expanduser("~/h2o-bench-server.log"), "w"))
peak = {"vram": baseline, "rss": 0}
stop = threading.Event()
def sample():
    while not stop.is_set():
        try:
            peak["vram"] = max(peak["vram"], gpu_used()); peak["rss"] = max(peak["rss"], rss_mib(child.pid) or 0)
        except Exception:
            pass
        time.sleep(0.2)
threading.Thread(target=sample, daemon=True).start()
try:
    while True:
        try:
            urllib.request.urlopen(f"http://127.0.0.1:{PORT}/health", timeout=2); break
        except Exception:
            if child.poll() is not None:
                sys.exit(f"llama-server exited with {child.returncode}; see ~/h2o-bench-server.log")
            time.sleep(0.5)
    print(f"server: -ngl {ngl} -ub {ub}, ready in {time.time() - started:.1f}s, VRAM +{gpu_used() - baseline} MiB", flush=True)

    # startup label check, as verify_labels(): " A".." P" are one distinct token each
    probe = render('{"x":1}', "Which?", [(f"o{i}", f"d{i}") for i in range(16)])
    base = tokenize(probe)
    label_ids = []
    for label in LABELS:
        full = tokenize(f"{probe} {label}")
        assert len(full) == len(base) + 1 and full[:len(base)] == base and full[-1] not in label_ids
        label_ids.append(full[-1])

    record = open(record_path).read()
    q1 = ("Which programming language is most of this project written in?", [(k, k) for k in ("rust", "python", "typescript", "go")])
    q2 = ("Does this project run a WebSocket server for miners?", [("true", "the statement holds"), ("false", "it does not")])
    # largest record prefix whose rendered prompt stays within the 40960-token limit
    lo, hi = 0, len(record)
    while hi - lo > 200:
        mid = (lo + hi) // 2
        if len(tokenize(render(record[:mid], *q1))) <= CTX - 40: lo = mid
        else: hi = mid
    record = record[:lo]

    def ask(name, question):
        instructions, options = question
        t0 = time.time()
        tokens = tokenize(render(record, instructions, options))
        t1 = time.time()
        ids = label_ids[:len(options)]
        mask = ",".join(f"[{i},false]" for i in range(VOCAB) if i not in set(ids))
        body = ('{"prompt":' + json.dumps(tokens) + ',"logit_bias":[' + mask + '],"n_predict":1,"n_probs":' + str(len(ids)) +
                ',"samplers":[],"post_sampling_probs":true,"cache_prompt":true,"stream":false,"temperature":1.0,'
                '"backend_sampling":false,"mirostat":0,"seed":0,"response_fields":["completion_probabilities","tokens_evaluated","truncated"]}').encode()
        t2 = time.time()
        result = post("/completion", body)
        t3 = time.time()
        assert result["tokens_evaluated"] == len(tokens) and result["truncated"] is False, result
        probs = {p["id"]: p["prob"] for p in result["completion_probabilities"][0]["top_probs"]}
        answer = {options[i][0]: round(probs.get(label_ids[i], 0), 3) for i in range(len(options))}
        print(f"{name}: {len(tokens)} prompt tokens | tokenize {1000*(t1-t0):.0f} ms | completion {t3-t2:.1f} s | "
              f"total {t3-t0:.1f} s | body {len(body)/1e6:.1f} MB | {answer}", flush=True)

    ask("question 1 (cold, 40k)", q1)
    ask("question 2 (same record, new question)", q2)
    ask("question 1 again (fully cached)", q1)
finally:
    stop.set()
    child.terminate(); child.wait()
    print(f"peak VRAM +{peak['vram'] - baseline} MiB, peak server RSS {peak['rss']} MiB", flush=True)
