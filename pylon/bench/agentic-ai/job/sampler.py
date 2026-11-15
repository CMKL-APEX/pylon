# SPDX-License-Identifier: Apache-2.0
#
# Copyright © 2026 CMKL-APEX

"""Samples the guest once a second: CPU time by kind (steal included), the
run-queue wait summed over CPUs (/proc/schedstat), GPU utilisation, and the
CPU time of the busiest processes."""

import json
import os
import threading
import time

try:
    import pynvml
except ImportError:  # pragma: no cover
    pynvml = None

HZ = os.sysconf("SC_CLK_TCK")


def cpu_times():
    with open("/proc/stat") as f:
        v = [int(x) for x in f.readline().split()[1:]]
    keys = ["user", "nice", "system", "idle", "iowait", "irq", "softirq", "steal"]
    return dict(zip(keys, v))


def run_delay_ns():
    total = 0
    with open("/proc/schedstat") as f:
        for line in f:
            if line.startswith("cpu"):
                total += int(line.split()[8])
    return total


def proc_cpu():
    out = {}
    for pid in os.listdir("/proc"):
        if not pid.isdigit():
            continue
        try:
            with open(f"/proc/{pid}/stat") as f:
                s = f.read()
            comm = s[s.index("(") + 1: s.rindex(")")]
            fields = s[s.rindex(")") + 2:].split()
            out[int(pid)] = (comm, int(fields[11]) + int(fields[12]))
        except (OSError, ValueError):
            pass
    return out


class Sampler(threading.Thread):
    def __init__(self, path, extra_pids=None, interval=1.0):
        super().__init__(daemon=True)
        self.path = path
        self.interval = interval
        self._halt = threading.Event()
        self.handle = None
        if pynvml:
            try:
                pynvml.nvmlInit()
                self.handle = pynvml.nvmlDeviceGetHandleByIndex(0)
            except Exception:
                self.handle = None

    def stop(self):
        self._halt.set()
        self.join(5)

    def run(self):
        prev_t, prev_c, prev_d, prev_p = time.time(), cpu_times(), run_delay_ns(), proc_cpu()
        with open(self.path, "w", buffering=1) as f:
            while not self._halt.wait(self.interval):
                t, c, d, p = time.time(), cpu_times(), run_delay_ns(), proc_cpu()
                dt = t - prev_t
                rec = dict(t=t, dt=dt,
                           cpu={k: (c[k] - prev_c[k]) / HZ / dt for k in c},
                           runq_wait=(d - prev_d) / 1e9 / dt)
                busy = []
                for pid, (comm, ticks) in p.items():
                    if pid in prev_p:
                        u = (ticks - prev_p[pid][1]) / HZ / dt
                        if u >= 0.05:
                            busy.append((round(u, 3), comm, pid))
                rec["procs"] = sorted(busy, reverse=True)[:12]
                if self.handle is not None:
                    try:
                        u = pynvml.nvmlDeviceGetUtilizationRates(self.handle)
                        rec["gpu"] = u.gpu
                        rec["gpu_mem_bw"] = u.memory
                    except Exception:
                        pass
                f.write(json.dumps(rec) + "\n")
                prev_t, prev_c, prev_d, prev_p = t, c, d, p
