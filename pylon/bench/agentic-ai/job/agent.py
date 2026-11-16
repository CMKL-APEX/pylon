#!/opt/venv/bin/python
# SPDX-License-Identifier: Apache-2.0
#
# Copyright © 2026 CMKL-APEX

"""Agentic inference load: vLLM serving a 7B model, and N concurrent agent
sessions. Each session runs tasks of K steps: an LLM call over a growing
context, then a real CPU tool. The tool sequence is seeded, so every scenario
replays the same mix. Writes steps.jsonl, tasks.jsonl and samples.jsonl."""

import argparse
import asyncio
import glob
import json
import os
import random
import resource
import subprocess
import sys
import time
import urllib.request

from openai import AsyncOpenAI

sys.path.insert(0, "/job")
from sampler import Sampler  # noqa: E402

PY = "/opt/venv/bin/python"
CLICK_TESTS = sorted(glob.glob("/corpus/click/tests/test_*.py"))
CPYTHON_FILES = sorted(
    f for f in glob.glob("/corpus/cpython/Lib/*.py") if 12_000 < os.path.getsize(f) < 40_000
)
RG_PATTERNS = ["def __init__", "raise ValueError", "import os", "TODO", "self\\.\\w+ = None",
               "async def", "except Exception", "return None", "@property", "lambda"]
# name, weight. "light": quick tools (one test file, one -O0 compile).
# "heavy": what coding agents run: the whole test suite, a parallel build.
TOOL_MIXES = {
    "light": [("read_file", 30), ("rg", 25), ("pytest", 25), ("pandas", 10), ("gcc", 10)],
    "heavy": [("read_file", 15), ("rg", 20), ("pytest_all", 30), ("pandas_big", 15),
              ("build", 20)],
}
SYSTEM = ("You are a software engineering agent working in a Python repository. "
          "Think briefly, then call exactly one tool by answering with a JSON object "
          '{"tool": name, "args": {...}}. Tools: read_file(path), rg(pattern), '
          "pytest(path), pandas(script), gcc(file).")


def tool_cmd(name, rng, sid):
    if name == "read_file":
        return ["cat", rng.choice(CPYTHON_FILES)]
    if name == "rg":
        return ["rg", "-n", "-j", "4", "--max-count", "3", rng.choice(RG_PATTERNS), "/corpus/cpython"]
    if name == "pytest":
        return [PY, "-m", "pytest", "-q", "-x", "-p", "no:cacheprovider", rng.choice(CLICK_TESTS)]
    if name == "pandas":
        return [PY, "/job/pandas_tool.py", str(rng.randrange(1 << 30))]
    if name == "gcc":
        return ["gcc", "-O0", "-c", "/corpus/sqlite/sqlite3.c", "-o", f"/tmp/sq{sid}.o"]
    if name == "pytest_all":
        return [PY, "-m", "pytest", "-q", "-p", "no:cacheprovider", "/corpus/click/tests"]
    if name == "pandas_big":
        return [PY, "/job/pandas_tool.py", str(rng.randrange(1 << 30)), "4000000"]
    if name == "build":
        # Four translation units at once, as `make -j4` would.
        return ["bash", "-c", "for i in 1 2 3 4; do gcc -O1 -DSQLITE_BUILD_UNIT=$i -c "
                f"/corpus/sqlite/sqlite3.c -o /tmp/sq{sid}_$i.o & done; wait"]
    raise ValueError(name)


async def run_tool(cmd):
    t0 = time.time()
    proc = await asyncio.create_subprocess_exec(
        *cmd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, cwd="/corpus/click")
    out, _ = await proc.communicate()
    t1 = time.time()
    return t0, t1, out.decode(errors="replace")


def children_cpu():
    r = resource.getrusage(resource.RUSAGE_CHILDREN)
    return r.ru_utime + r.ru_stime


async def llm_call(client, model, messages, max_tokens, seed):
    t0 = time.time()
    ttft = None
    text = []
    usage = None
    stream = await client.chat.completions.create(
        model=model, messages=messages, max_tokens=max_tokens, temperature=0.7, seed=seed,
        stream=True, stream_options={"include_usage": True})
    async for chunk in stream:
        if chunk.choices and chunk.choices[0].delta.content:
            if ttft is None:
                ttft = time.time()
            text.append(chunk.choices[0].delta.content)
        if chunk.usage:
            usage = chunk.usage
    t1 = time.time()
    return t0, ttft or t1, t1, "".join(text), usage


async def session(sid, args, client, model, t_start, t_end, steps_f, tasks_f):
    rng = random.Random(args.seed * 1000 + sid)
    mix = TOOL_MIXES[args.tools]
    names = [n for n, _ in mix]
    weights = [w for _, w in mix]
    task = 0
    # Stagger session starts over the first think time.
    await asyncio.sleep(rng.uniform(0, args.think_s))
    while time.time() < t_end:
        task_rng = random.Random(rng.randrange(1 << 30))
        context_file = task_rng.choice(CPYTHON_FILES)
        with open(context_file, errors="replace") as f:
            ctx = f.read()[: args.context_chars]
        messages = [{"role": "system", "content": SYSTEM},
                    {"role": "user", "content": f"Fix the failing behaviour in {context_file}.\n\n{ctx}"}]
        t_task = time.time()
        llm_s = tool_s = 0.0
        for step in range(args.steps):
            if time.time() >= t_end:
                break
            seed = sid * 100003 + task * 101 + step
            l0, lfirst, l1, text, usage = await llm_call(
                client, model, messages, args.max_tokens, seed)
            name = task_rng.choices(names, weights)[0]
            cmd = tool_cmd(name, task_rng, sid)
            t0, t1, out = await run_tool(cmd)
            llm_s += l1 - l0
            tool_s += t1 - t0
            rec = dict(sid=sid, task=task, step=step, llm_start=l0, ttft=lfirst - l0,
                       llm_s=l1 - l0, prompt_tokens=usage.prompt_tokens if usage else None,
                       completion_tokens=usage.completion_tokens if usage else None,
                       tool=name, tool_start=t0, tool_s=t1 - t0, measured=l0 >= t_start)
            steps_f.write(json.dumps(rec) + "\n")
            messages.append({"role": "assistant", "content": text})
            messages.append({"role": "user", "content": f"Tool output:\n{out[-args.tool_out_chars:]}"})
        tasks_f.write(json.dumps(dict(sid=sid, task=task, start=t_task, end=time.time(),
                                      steps=step + 1, llm_s=llm_s, tool_s=tool_s,
                                      measured=t_task >= t_start)) + "\n")
        task += 1
        # The user reads the result before the next task: an idle window.
        await asyncio.sleep(rng.expovariate(1 / args.think_s))


def wait_health(url, timeout):
    t_end = time.time() + timeout
    while time.time() < t_end:
        try:
            with urllib.request.urlopen(url, timeout=2) as r:
                if r.status == 200:
                    return True
        except Exception:
            pass
        time.sleep(2)
    return False


async def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--params", default="/job/params.json")
    a = ap.parse_args()
    p = json.load(open(a.params))
    args = argparse.Namespace(**{**dict(sessions=32, steps=8, max_tokens=256, think_s=5.0,
                                       context_chars=24000, tool_out_chars=3000, seed=1, tools="light",
                                       warmup_s=60, duration_s=300, measure_start=None,
                                       gpu_mem=0.85, max_model_len=32768), **p.get("agent", {})})
    out = "/job/out"
    os.makedirs(out, exist_ok=True)
    model = "/models/Qwen2.5-7B-Instruct"
    vllm_log = open(f"{out}/vllm.log", "w")
    vllm = subprocess.Popen(
        [PY, "-m", "vllm.entrypoints.openai.api_server", "--model", model, "--port", "8000",
         "--max-model-len", str(args.max_model_len), "--gpu-memory-utilization", str(args.gpu_mem),
         "--enable-prefix-caching", "--disable-log-requests"],
        stdout=vllm_log, stderr=subprocess.STDOUT,
        # No nvcc in the guest: FlashInfer's JIT kernels can't build.
        env={**os.environ, "VLLM_USE_FLASHINFER_SAMPLER": "0"})
    if not wait_health("http://127.0.0.1:8000/health", 900):
        print("vLLM did not start", flush=True)
        vllm.kill()
        sys.exit(1)
    print(f"vLLM up at {time.time():.3f}", flush=True)
    sampler = Sampler(f"{out}/samples.jsonl", extra_pids={"vllm": vllm.pid})
    sampler.start()
    client = AsyncOpenAI(base_url="http://127.0.0.1:8000/v1", api_key="x", timeout=600)
    now = time.time()
    t_start = args.measure_start if args.measure_start else now + args.warmup_s
    if t_start < now:
        print(f"measure_start already passed by {now - t_start:.1f} s", flush=True)
    t_end = t_start + args.duration_s
    json.dump(dict(t_start=t_start, t_end=t_end, args=vars(args), cpu_count=os.cpu_count()),
              open(f"{out}/meta.json", "w"))
    cpu0 = children_cpu()
    with open(f"{out}/steps.jsonl", "w", buffering=1) as steps_f, \
            open(f"{out}/tasks.jsonl", "w", buffering=1) as tasks_f:
        await asyncio.gather(*(session(s, args, client, model, t_start, t_end, steps_f, tasks_f)
                               for s in range(args.sessions)))
    print(f"tool cpu s: {children_cpu() - cpu0:.1f}", flush=True)
    try:
        with urllib.request.urlopen("http://127.0.0.1:8000/metrics", timeout=10) as r:
            open(f"{out}/vllm-metrics.txt", "wb").write(r.read())
    except Exception as e:
        print(f"metrics: {e}", flush=True)
    sampler.stop()
    vllm.terminate()
    try:
        vllm.wait(60)
    except subprocess.TimeoutExpired:
        vllm.kill()


if __name__ == "__main__":
    asyncio.run(main())
