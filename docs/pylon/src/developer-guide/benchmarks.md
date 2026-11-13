# Performance and benchmarks

This page collects Pylon's performance figures: what was run, where,
when and how, and how to run it again. There are few so far. Pylon has
**no measurement of a real core handoff between real VMs yet**: that is
step S6 of [Enhancements](enhancements.md#implementation-steps)
([#53](https://github.com/CMKL-APEX/pylon/issues/53)).

## How to read the numbers

- Every result is from one date on one machine. It is a data point, not
  a guarantee.
- **Modelled** figures come from a program that imitates part of the
  system. They show what the model predicts, not what a VM sees.
- Defaults and limits (the 8 µs grace period, the 1,000-cycle pause) are
  configuration, not measurements.

## pylon-bench: Pylon parking vs. modelled ACPI hotplug

`pylon-bench` (`pylon/pylond/src/bin/pylon_bench.rs`) runs the same
CPU-bound workload twice, scaling between 1 and 8 workers in bursts:

- **Pylon:** worker threads stand in for vCPUs. They park and wake
  through a real shared page, a real `MatrixEngine` and `eventfd`s,
  exactly as vCPU threads would, but **no VM or KVM is involved**.
- **ACPI hotplug:** a model of Cloud Hypervisor's hotplug path (thread
  creation, a barrier, the `_SB_.PRES` register scan) plus **fixed
  guest-side delays it injects**: 2,500 µs to bring a CPU online and
  1,500 µs to take one offline, by default (`--acpi-bringup-us`,
  `--acpi-eject-us`). Those delays aren't measured. They are estimates
  of the guest's CPU hotplug path, and they dominate the ACPI column.

### Result (modelled, 2026-10-29)

`cargo run --release -p pylond --bin pylon-bench` (defaults: 20 waves,
1 ↔ 8 workers, 64 tasks of 25,000 iterations per wave). Both runs
computed the same checksum.

| Metric | Pylon | ACPI hotplug (model) |
|---|---|---|
| Scale-out latency, mean | 138 µs | 3,152 µs |
| Scale-out latency, p50 | 136 µs | 3,121 µs |
| Scale-out latency, p99 | 173 µs | 3,290 µs |
| Scale-in latency, mean | 30 µs | 1,869 µs |
| Scale-in latency, p99 | 44 µs | 1,910 µs |
| Workload wall time | 70.6 ms | 158.6 ms |

| | |
|---|---|
| CPU | AMD EPYC 9124, 16 cores, 32 threads, 1 socket, 1 NUMA node (no `WAITPKG`: the engine used `PAUSE`) |
| Memory | 31,293 MiB |
| Kernel | Ubuntu `7.0.0-42-generic`, with `scx_pylon` attached by a running `pylond` |
| Build | Release profile; `pylond` as on `main` at `e679f1133` |

**What it does and doesn't show.** The Pylon column is the time to wake
and park threads through the real engine and `eventfd`s on a busy host.
Pylon's latency budget is a few microseconds, and 138 µs is far above
it. The reason hasn't been investigated
([#54](https://github.com/CMKL-APEX/pylon/issues/54)): thread scheduling without
`scx_pylon` assignments for these worker threads, the engine sharing
CPUs with the workload, and the benchmark's own polling are all
candidates. The ACPI column is mostly the injected delays, so the ratio
between the columns follows from the model's assumptions. Don't quote it
as a measured speed-up.

### Result (modelled, 2026-10-30)

The same command and defaults, run three times in a row. Every run
computed the same checksum for both columns.

| Metric | Pylon, run 1 / 2 / 3 | ACPI hotplug (model), run 1 / 2 / 3 |
|---|---|---|
| Scale-out latency, mean | 22.4 / 26.0 / 22.2 µs | 3,498 / 3,563 / 3,365 µs |
| Scale-out latency, p50 | 21.1 / 24.8 / 18.4 µs | 3,504 / 3,569 / 3,355 µs |
| Scale-out latency, p99 | 49.3 / 39.8 / 66.8 µs | 3,665 / 3,602 / 3,528 µs |
| Scale-in latency, mean | 1.73 / 2.75 / 1.63 µs | 1,706 / 1,679 / 1,696 µs |
| Scale-in latency, p99 | 2.44 / 17.4 / 2.05 µs | 1,751 / 1,747 / 1,860 µs |
| Workload wall time | 14.9 / 15.0 / 14.9 ms | 119.0 / 119.7 / 115.8 ms |

| | |
|---|---|
| Machine | NVIDIA DGX A100 |
| CPU | 2 × AMD EPYC 7742, 64 cores and 128 threads each, 8 NUMA nodes (no `WAITPKG`: the engine used `PAUSE`) |
| Memory | 1,031,823 MiB |
| Kernel | Ubuntu `6.8.0-138-generic`, no `sched_ext` (so no `scx_pylon`); load average about 2 |
| Build | Release profile; `main` at `e5c3a7660` |

**What it does and doesn't show.** Pylon's scale-out is about 6× and
its scale-in about 17× faster than on 2026-10-29. That comparison
changes the host, the kernel and the code at once: this machine has 256
mostly idle threads for 8 workers and the engine, and the build is 42
commits newer. Neither run had `scx_pylon` managing the worker threads.
Running this build on the EPYC 9124 host would separate the code from
the host. Scale-in is now within the 3–6 µs target, and scale-out is
still about four times over it. With 20 waves per run, each p99 is the
run's slowest wave.

### `scx_pylon`'s cost to the benchmark (modelled, 2026-10-30 and 2026-11-02)

`scx_pylon` makes the Pylon column slower. The benchmark's worker
threads aren't vCPUs, so to `scx_pylon` they are host tasks, and each
wake of one goes through `scx_pylon`'s host path: `enqueue`, the shared
host queue, then `dispatch` with its claim checks. Without `sched_ext`
the kernel wakes them directly.

**On the EPYC 9124 host (2026-10-30).** One `pylon-bench` build from
`main`, with sched_ext off or with `scx_pylon` from one of two commits
attached by a test `pylond`. Means, rounded. The raw results were lost
when the host restarted, so only these figures remain.

| Configuration | Scale-out, mean | Scale-in, mean |
|---|---|---|
| sched_ext off, with or without `pylond` | about 20 µs | about 2.5 µs |
| `scx_pylon` at `e679f1133` (the 2026-10-29 build) | about 55 µs | about 12 µs |
| `scx_pylon` at `main` then (after #16, #17 and #21) | about 65 µs | about 24 µs |

Most of the cost is `scx_pylon` itself. The rest came with #16, which
sent every waking host thread through the host queue and `dispatch`;
before it, the kernel's global queue took them without calling into
BPF. Earlier runs that day were 4 to 5 times slower in every
configuration, sched_ext off too, because of other load on the host.

**A fast path, tried in the dev VM (2026-11-02).** The change: in
`select_cpu`, a host task that wakes onto an idle CPU whose siblings
allow host work is inserted on that CPU's local queue, skipping the host
queue and `dispatch`. Six interleaved rounds of `pylon-bench` (defaults)
in a 16-vCPU KVM guest on the same host (kernel `7.0.0-38-generic`), one
bench build, `scx_pylon` attached by a test `pylond` (`--cores 2-5`).
Medians of the six runs:

| Configuration | Scale-out, mean | Scale-out, p99 | Scale-in, mean | Scale-in, p99 |
|---|---|---|---|---|
| sched_ext off | 62 µs | 147 µs | 4.3 µs | 11 µs |
| `scx_pylon` at `4d7106ae5` (spec 08 P1) | 108 µs | 155 µs | 95 µs | 1,040 µs |
| The same with the fast path | 311 µs | 3,184 µs | 122 µs | 1,858 µs |

The fast path didn't help. Its scale-out was slower in five of the six
rounds and about the same in the other; its scale-in was faster in some
rounds and much slower in others. It wasn't merged. A likely reason is
that a task inserted this way runs a default 20 ms slice on a CPU that
another task may want soon. The guest's own vCPUs halt and wake through the
host, so every figure here is noisier and higher than on the host
itself: single runs of sched_ext off reached 252 µs out and 155 µs in.
Read the table for its order, not its values.

**Not known yet:** the cost for a real vCPU, which `scx_pylon` places
directly instead of through the host queue. These threads are a proxy
for host tasks, which is the open gap below.

## Other figures

| Figure | Kind | Source |
|---|---|---|
| Grace period: 8 µs | Default (`--revoke-grace-us`) | `REVOKE_GRACE_PERIOD_US`, `pylond/src/engine.rs` |
| Forced handoff: 50 µs after the first signal | Default (`--force-handoff-us`) | `FORCE_HANDOFF_US` |
| Pause between sweeps: 333 ns, converted to counter ticks with the host rate (about 1,000 ticks on a 3 GHz TSC) | Default | `SWEEP_INTERVAL_NS` |
| Handoff target: "~3–6 µs" | Design goal | `spec/00-architecture.md` §1 |

## Gaps

These haven't been measured and are needed before Pylon's latency can
be claimed:

- The engine's sweep time, by number of VMs.
- Kick to `PARKED`, and revocation to recipient running, between real
  VMs, with and without `scx_pylon` (step S6,
  [#53](https://github.com/CMKL-APEX/pylon/issues/53)). `pylond` now
  records revocation-to-`PARKED` latency per VM
  (`pylon_vm_park_latency_microseconds`), but no run has been recorded
  yet.
- Wake latency of a parked vCPU in a real VM.
- For parking inside KVM: wake-to-run latency for IPIs to parked vCPUs,
  guest throughput against the `eventfd` park, and halt-polling
  settings (spec 08 P3,
  [#50](https://github.com/CMKL-APEX/pylon/issues/50)). Spec 08 §7
  records stall and preemption counts from its prototype runs, not
  latencies.
- `scx_pylon`'s cost for non-VM tasks on the host, beyond the
  benchmark's threads above.
- The same `pylon-bench` run on a CPU with `WAITPKG`.

When you add a figure, add it here with its date, hardware, build and
command, and say whether it was measured or modelled.

## Running the benchmark

```bash
cargo run --release -p pylond --bin pylon-bench            # the runs above
cargo run --release -p pylond --bin pylon-bench -- --quick # short preset used by CI
cargo run --release -p pylond --bin pylon-bench -- --help  # all options
```
