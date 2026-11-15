#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
#
# Copyright © 2026 CMKL-APEX

"""Host side: once a second, every thread of the given VMM pids: its name, CPU
time and run delay (time runnable but not running), from schedstat."""
import json, os, sys, time
out, pids = sys.argv[1], [int(x) for x in sys.argv[2].split(",")]
names = sys.argv[3].split(",")
def snap():
    s = {}
    for pid, vm in zip(pids, names):
        try:
            tids = os.listdir(f"/proc/{pid}/task")
        except OSError:
            continue
        for tid in tids:
            try:
                comm = open(f"/proc/{pid}/task/{tid}/comm").read().strip()
                run, wait, n = map(int, open(f"/proc/{pid}/task/{tid}/schedstat").read().split())
                s[(vm, int(tid))] = (comm, run, wait, n)
            except (OSError, ValueError):
                pass
    return s
prev, pt = snap(), time.time()
with open(out, "w", buffering=1) as f:
    while any(os.path.exists(f"/proc/{p}") for p in pids):
        time.sleep(1)
        cur, t = snap(), time.time()
        dt = t - pt
        th = []
        for k, (comm, run, wait, n) in cur.items():
            if k in prev:
                _, r0, w0, n0 = prev[k]
                if run - r0 or wait - w0:
                    th.append([k[0], comm, round((run - r0) / 1e9 / dt, 4), round((wait - w0) / 1e9 / dt, 4), n - n0])
        f.write(json.dumps(dict(t=t, threads=th)) + "\n")
        prev, pt = cur, t
