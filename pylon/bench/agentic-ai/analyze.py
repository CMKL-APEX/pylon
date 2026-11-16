#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
#
# Copyright © 2026 CMKL-APEX

"""Summarise benchmark scenarios: analyze.py OUT_DIR SCENARIO..."""
import json
import os
import statistics as st
import sys
from collections import defaultdict


def load(path):
    if not os.path.exists(path):
        return []
    out = []
    for line in open(path):
        try:
            out.append(json.loads(line))
        except json.JSONDecodeError:
            pass
    return out


def pct(v, q):
    if not v:
        return float("nan")
    v = sorted(v)
    return v[min(len(v) - 1, int(q / 100 * len(v)))]


def window(meta):
    return meta["t_start"], meta["t_end"]


def summarise(base, name):
    d = os.path.join(base, name)
    r = {"scenario": name}
    am = os.path.join(d, "agent", "meta.json")
    if os.path.exists(am):
        t0, t1 = window(json.load(open(am)))
        steps = [s for s in load(os.path.join(d, "agent", "steps.jsonl"))
                 if t0 <= s["llm_start"] < t1]
        tasks = [t for t in load(os.path.join(d, "agent", "tasks.jsonl"))
                 if t["measured"] and t["end"] <= t1 + 1 and t["steps"] == 8]
        r["agent_tasks"] = len(tasks)
        r["agent_steps"] = len(steps)
        r["agent_steps_per_min"] = len(steps) / (t1 - t0) * 60
        tt = [t["end"] - t["start"] for t in tasks]
        r["task_s_p50"], r["task_s_p99"] = pct(tt, 50), pct(tt, 99)
        llm = [s["llm_s"] for s in steps]
        ttft = [s["ttft"] for s in steps]
        r["llm_s_p50"], r["llm_s_p99"] = pct(llm, 50), pct(llm, 99)
        r["ttft_p50"], r["ttft_p99"] = pct(ttft, 50), pct(ttft, 99)
        tot_llm = sum(llm)
        tot_tool = sum(s["tool_s"] for s in steps)
        r["tool_share"] = tot_tool / (tot_llm + tot_tool) if steps else float("nan")
        bytool = defaultdict(list)
        for s in steps:
            bytool[s["tool"]].append(s["tool_s"])
        r["tools"] = {k: (len(v), round(pct(v, 50), 3), round(pct(v, 99), 3))
                      for k, v in sorted(bytool.items())}
        samp = [s for s in load(os.path.join(d, "agent", "samples.jsonl")) if t0 <= s["t"] < t1]
        r["agent_gpu"] = st.mean(s.get("gpu", 0) for s in samp) if samp else None
        r["agent_steal"] = st.mean(s["cpu"]["steal"] for s in samp) if samp else None
        r["agent_busy_cpus"] = st.mean(sum(v for k, v in s["cpu"].items()
                                           if k not in ("idle", "iowait", "steal"))
                                       for s in samp) if samp else None
        r["agent_runq_wait"] = st.mean(s["runq_wait"] for s in samp) if samp else None
        top = defaultdict(float)
        for s in samp:
            for u, comm, _pid in s["procs"]:
                top[comm] += u / len(samp)
        r["agent_top_procs"] = sorted(((round(v, 2), k) for k, v in top.items()), reverse=True)[:6]
    tm = os.path.join(d, "trainer", "meta.json")
    if os.path.exists(tm):
        t0, t1 = window(json.load(open(tm)))
        steps = [s for s in load(os.path.join(d, "trainer", "train_steps.jsonl"))
                 if t0 <= s["t"] < t1]
        if steps:
            span = steps[-1]["t"] + steps[-1]["data_wait"] + steps[-1]["compute"] - steps[0]["t"]
            r["train_tok_s"] = sum(s["tokens"] for s in steps) / span
            r["train_data_wait_share"] = sum(s["data_wait"] for s in steps) / span
            r["train_compute_p50"] = pct([s["compute"] for s in steps], 50)
        samp = [s for s in load(os.path.join(d, "trainer", "samples.jsonl")) if t0 <= s["t"] < t1]
        r["train_gpu"] = st.mean(s.get("gpu", 0) for s in samp) if samp else None
        r["train_steal"] = st.mean(s["cpu"]["steal"] for s in samp) if samp else None
        r["train_busy_cpus"] = st.mean(sum(v for k, v in s["cpu"].items()
                                           if k not in ("idle", "iowait", "steal"))
                                       for s in samp) if samp else None
    host = load(os.path.join(d, "host.jsonl"))
    # Host: per VM, CPU used by vCPU threads and their run delay (waiting for a core).
    m = os.path.join(d, "agent", "meta.json") if os.path.exists(am) else tm
    if host and os.path.exists(m):
        t0, t1 = window(json.load(open(m)))
        hs = [h for h in host if t0 <= h["t"] < t1]
        agg = defaultdict(float)
        for h in hs:
            for vm, comm, run, wait, _n in h["threads"]:
                kind = "vcpu" if comm.startswith("vcpu") else "other"
                agg[(vm, kind, "run")] += run / len(hs)
                agg[(vm, kind, "wait")] += wait / len(hs)
        r["host"] = {f"{vm}.{kind}.{x}": round(v, 2) for (vm, kind, x), v in sorted(agg.items())}
    return r


def main():
    base = sys.argv[1]
    for name in sys.argv[2:]:
        r = summarise(base, name)
        print(json.dumps(r, indent=1, default=str))


if __name__ == "__main__":
    main()
