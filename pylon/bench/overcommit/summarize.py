#!/usr/bin/env python3
#
# Copyright © 2026 CMKL-APEX
#
# SPDX-License-Identifier: Apache-2.0
#
# Summarizes the runs run.sh collected: <out>/<scenario>/<system>-<rep>/.
# Throughput is each VM's work per second over the loads' window; CPUs is
# the host CPU the VMs' threads used in that window (plus pylond's, shown
# apart). The last column compares each system with upstream sharing the
# CPUs: each VM's work over upstream's median for that VM, averaged over the
# two VMs, median of runs.
# Usage: summarize.py <out>

import pathlib
import re
import statistics
import sys


def parse(run):
    res = {}
    for line in (run / "result.txt").read_text().split():
        if "=" in line:
            k, v = line.split("=", 1)
            res[k] = v
    for vm in (1, 2):
        log = run / f"serial{vm}.log"
        text = log.read_text(errors="replace") if log.exists() else ""
        m = re.search(r"PYLONBENCH (\w+) result (.*)", text)
        if m:
            res[f"vm{vm}"] = dict(kv.split("=") for kv in m.group(2).split())
            res[f"vm{vm}"]["load"] = m.group(1)
    return res


def fmt(x):
    return f"{x / 1e6:.2f}M" if x >= 1e6 else f"{x / 1e3:.1f}k" if x >= 1e3 else f"{x:.0f}"


def main(out):
    out = pathlib.Path(out)
    print("| Scenario | System | VM 1 | VM 2 | VM CPUs | pylond CPUs | Lockups | vs upstream |")
    print("|---|---|---|---|---|---|---|---|")
    for scenario in sorted(p for p in out.iterdir() if p.is_dir()):
        runs = {}
        for run in sorted(scenario.iterdir()):
            if (run / "result.txt").exists():
                runs.setdefault(run.name.rsplit("-", 1)[0], []).append(parse(run))
        base = {}
        for vm in ("vm1", "vm2"):
            ups = [float(r[vm]["ops_per_s"]) for r in runs.get("upstream", []) if "ops_per_s" in r.get(vm, {})]
            if ups:
                base[vm] = statistics.median(ups)
        for system in ("upstream", "dedicated", "pylon"):
            rs = runs.get(system, [])
            if not rs:
                continue
            cells = []
            for vm in ("vm1", "vm2"):
                vals = []
                for r in rs:
                    d = r.get(vm)
                    if d is None:
                        vals.append("none")
                    elif "ops_per_s" in d:
                        vals.append(fmt(float(d["ops_per_s"])) + "/s")
                    elif "p50_us" in d:
                        vals.append(f"{d['p50_us']}/{d['p99_us']}/{d['p999_us']} us")
                    else:
                        vals.append("no samples")
                cells.append(", ".join(vals))
            vmcpu = ", ".join(f"{float(r.get('cpu_vm1', 'nan')) + float(r.get('cpu_vm2', 'nan')):.2f}" for r in rs)
            pcpu = ", ".join(r.get("cpu_pylond", "-") for r in rs)
            lock = ", ".join(r.get("lockups", "?") for r in rs)
            rel = []
            for r in rs:
                parts = [float(r[vm]["ops_per_s"]) / base[vm] for vm in base if "ops_per_s" in r.get(vm, {})]
                if parts and len(parts) == len(base):
                    rel.append(sum(parts) / len(parts))
            vs = f"{statistics.median(rel):.2f}x" if rel else "-"
            print(f"| {scenario.name} | {system} | {cells[0]} | {cells[1]} | {vmcpu} | {pcpu} | {lock} | {vs} |")


if __name__ == "__main__":
    main(sys.argv[1])
