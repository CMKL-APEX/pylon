# Performance and benchmarks

This page collects Pylon's performance figures: what was run, where,
when and how, and how to run it again. There are few so far. A core
handoff between real VMs takes about 100–125 µs at the median, against
a 3–6 µs target: see
[Handoff latency between real VMs](#handoff-latency-between-real-vms-measured-2026-11-13).

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

## aarch64 with real guests (measured, 2026-11-13)

Spec 07 A-T8. Two 4-vCPU Linux guests (`initial_active_vcpus=1`, the
guest agent polling every 1 ms) on three managed cores, `pylond` with
`scx_pylon` attached, 150 s runs with the first 40 s dropped. The loads
are spec 08 P3's: *work* (vCPUs 1 to 3 each do a fixed unit, then sleep
2 ms; units per 10 s) and *ping-pong* (two tasks on vCPUs 1 and 2 pass a
byte over pipes, 1 ms apart; round trip in µs). VM 2 is bursty (every
7 s, 100 ms of spinning on vCPUs 1 to 3), busy (spinning on every vCPU)
or idle. The baseline is VM 1 alone, without `--pylon`, pinned to the
same three cores. No guest ran `pv-pylon`, so every revocation was
forced.

| | |
|---|---|
| Host | aarch64 reference host: 20 cores (10 Cortex-X925, 10 Cortex-A725), no SMT, one NUMA node |
| Kernel | `7.0.0-1024-nvidia`, with `sched_ext`; cpuidle `LPI-0` and `LPI-1` (133 µs exit latency) |
| Guest | Cloud Hypervisor's pinned kernel (`ch-release-v7.2.8-20261009`) and a busybox initramfs |
| `pylond` | `--cores 5-7 --engine-cpu 8` (X925 cores) unless noted |

**Work units per 10 s, VM 1** (median of the 10 s samples; three runs):

| Setup | Units per 10 s | Stalls |
|---|---|---|
| Baseline | 13,198, 13,155, 13,114 | none |
| `park=kvm`, VM 2 bursty | 13,048, 13,068, 13,049 | none |
| `park=kvm`, VM 2 idle | 13,020, 13,052, 13,053 | none |
| `park=kvm`, VM 2 busy | 13,330, 13,326, 13,326 | none |
| `park=vmm`, VM 2 bursty | 8,690, 8,710, 8,658 | none logged |
| `park=vmm`, VM 2 idle | 6,662, 8,681, 6,204 | none logged |
| `park=vmm`, VM 2 busy | stopped, 4,298, stopped | VM 1 stopped making progress |

**Ping-pong round trip, µs** (p50 / p99; two runs):

| Setup | Run 1 | Run 2 |
|---|---|---|
| Baseline | 224 / 512 | 226 / 506 |
| Baseline, cores held out of deep idle | 10 / 12 | 10 / 12 |
| `park=kvm`, VM 2 idle | 20 / 588 | 20 / 397 |
| `park=kvm`, VM 2 busy | 38 / 90 | 39 / 97 |
| `park=kvm`, VM 2 busy, cores held out of deep idle | 23 / 1,116 | 23 / 1,117 |
| `park=kvm`, VM 2 busy, engine on an A725 core | 31 / 92 | 34 / 98 |
| `park=vmm`, any | no samples | no samples |

**Forced park of a real vCPU** (`bench_forced_park_latency_real_kvm_aarch64`,
3,000 iterations, µs, p50 / p90 / p99):

| | Request to `PARKED` | Wake to `RUNNING` |
|---|---|---|
| No idle limit, run 1 | 28.3 / 108.9 / 272.4 | 146.6 / 413.4 / 531.2 |
| No idle limit, run 2 | 16.6 / 108.7 / 233.3 | 9.4 / 697.1 / 732.2 |
| Cores held out of deep idle, run 1 | 15.2 / 17.2 / 20.3 | 9.4 / 10.2 / 11.2 |
| Cores held out of deep idle, run 2 | 15.1 / 17.1 / 19.6 | 10.0 / 10.7 / 11.6 |

"Cores held out of deep idle" is a PM QoS request of 0 on
`/dev/cpu_dma_latency` for the whole run.

**What it shows:**
- Parking inside KVM keeps an unmodified guest at the baseline's
  throughput, with no stall in 22 runs. Parking in the VMM strands the
  guest's tasks on parked vCPUs: throughput falls by a third to two
  thirds, the ping-pong never runs, and with the other VM busy VM 1
  stops. This is why `park=kvm` is the default on aarch64 as well.
- Idle-state exits are most of a wake's latency on this host. Holding
  the cores out of deep idle cuts the forced-park wake's p99 from 531
  to 732 µs to 11 to 12 µs, and the baseline ping-pong from 224 / 512
  to 10 / 12 µs.
- With the other VM busy and the cores held out of deep idle, the
  ping-pong's p99 rises to about 1.1 ms, against about 0.1 ms without
  the hold. Not explained yet; it may be the same kind of tail as the
  x86_64 ~1.9 ms tail ([#52](https://github.com/CMKL-APEX/pylon/issues/52)).
- The engine's core type (X925 or A725) made no measurable difference.
- VM 1's VMM used 32 to 41% of a CPU with `park=kvm`, against 27% for
  the baseline.

## Handoff latency between real VMs (measured, 2026-11-13)

Spec 06 step S6 ([#53](https://github.com/CMKL-APEX/pylon/issues/53)),
from `pylond`'s latency histograms
([Metrics](../reference/metrics.md)).

**Setup:** a bare-metal x86_64 host (AMD EPYC 7282, 16 cores with SMT,
Ubuntu 7.0.0-43 kernel), `pylond` with `scx_pylon` on 3 managed CPUs.
Two 4-vCPU Ubuntu guests park inside KVM (the default) and load
`pv-pylon`. VM 1 runs the spec 06 S5 load (two spinners and a `/proc`
reader on every CPU); VM 2 spins on 3 vCPUs for 100 ms every second, so
cores move about 200 times each way per run. 150 s runs, the first 40 s
dropped, two runs each; percentiles are read from the buckets. The
default grace (8 µs) makes every revocation forced; a 200 µs grace
(`--revoke-grace-us 200`) makes them cooperative.

| Interval, µs (p50 / p99) | Grace 8 µs (forced) | Grace 200 µs (cooperative) |
|---|---|---|
| Revocation request → donor `PARKED` | 55–67 / 64–72 (a few per run: most donors hadn't parked 50 µs after the signal) | 66–82 / 79–153 |
| Core assigned → recipient running | 38–48 / 57–140 | 42–47 / 59–78 |
| Revocation request → recipient running | 100–112 / 117–205 | 114–126 / 137–291 |

These are with the re-signal back-off below. The target is a handoff in
3–6 µs and a park within the 8 µs grace; both are an order of magnitude
off.

**Where the time goes** (from a host trace of signals, KVM exits and
system calls):

- The signal stops the vCPU's `KVM_RUN` within 3–6 µs.
- The VMM then takes 40–60 µs before it publishes `PARKED` (forced park)
  or re-enters the guest (wake). About 17 µs of it pass before the
  first signal handler returns; most of that is the trace's own probes
  (see [the profile below](#profile-without-kprobes-measured-2026-11-14)).
- `pylond` re-signalled a waking vCPU every 10 µs, so several signals
  were delivered during one wake. Backing the re-signals off from
  100 µs cut wake p50 from 53–58 to 42–46 µs and p99 from 88–124 to
  59–78 µs.
- The VMM's seccomp filter costs 5–10% (`--seccomp false`: wake p50
  47–61 µs, park p50 60–81 µs).
- Holding the cores out of deep idle (PM QoS 0) changed nothing on this
  host, unlike on the aarch64 reference host.
- A cooperative park takes about as long as a forced one: the guest's
  kick-to-doorbell path (interrupt, `SCHED_FIFO` park thread, doorbell
  exit) adds up to the same 60–80 µs
  ([#74](https://github.com/CMKL-APEX/pylon/issues/74)). Split up
  (2026-11-14, [spec 09 §9](https://github.com/CMKL-APEX/pylon/blob/main/spec/09-park-by-halt.md#9-h0-result-2026-11-14)):
  4–32 µs from the kick to its injection, 16–64 µs in the guest to the
  doorbell, and 8–16 µs for KVM to handle the doorbell's exit.

### Unmodified guests: kicking the new owner's CPU (measured, 2026-11-14)

[#54](https://github.com/CMKL-APEX/pylon/issues/54). **Setup:** a
bare-metal x86_64 host (AMD EPYC 9124, 16 cores with SMT, Ubuntu
7.0.0-42 kernel), `pylond` with `scx_pylon` on 3 managed CPUs and the
default grace. Two 4-vCPU Ubuntu guests park inside KVM, without
`pv-pylon`, and each spins on all 4 vCPUs, so every revocation is forced
and parked vCPUs keep waking to share their VM's CPUs. A script moves
cores from one VM to the other every 50 ms by swapping their
`max_cores` (1 and 3) over the control socket: about 590 handoffs per
run. A host trace (signals, KVM exits, system calls, context switches
on the managed CPUs) covered 20 s of each run.

| Interval, µs (p50 / p90 / p99) | Before | Kicking the new owner's CPU |
|---|---|---|
| Revocation request → donor `PARKED` | 52 / 60 / 64 | 52 / 60 / 64 |
| Core assigned → recipient running | 80 / 1,408 / 2,048 | 40 / 56 / 1,024 |
| Revocation request → recipient running | 144 / 1,408 / 2,304 | 104 / 120 / 1,152 |

**Where the time went:** most recipients (271 of 343 in the trace)
weren't on a CPU when `pylond` gave them a core: they were woken vCPUs
waiting for their turn on a CPU their VM shares. A signal doesn't queue
a thread that is already runnable, and `scx_pylon` doesn't watch the
owner table, so nothing told the new owner's CPU. When the donor parked
and blocked, the recipient ran within 20 µs (p50); when the donor kept
running (an interrupt woke it inside KVM at once), the recipient waited
for a tick, 662 µs at the median. `pylond` now kicks the CPU after each
assignment (`pylon_kick_owner`).

### Profile without kprobes (measured, 2026-11-14)

[#54](https://github.com/CMKL-APEX/pylon/issues/54). The setup above,
on `main` with the kick and with `SIGUSR2` as the kick signal
([#99](https://github.com/CMKL-APEX/pylon/pull/99)), one run. The
trace used tracepoints only (signals, system calls, context switches):
the earlier trace's kprobes fired about 16 times between a `KVM_RUN`
exit and the first handler's return, and inflated it.

| Interval, µs (p50 / p90 / p99) | Park (390) | Wake (390) |
|---|---|---|
| `KVM_RUN` exit → first handler's `rt_sigreturn` | 8 / 11 / 26 | 5 / 9 / 16 |
| `KVM_RUN` exit → next `KVM_RUN` | 45 / 151 / 1,951 | 18 / 25 / 32 |
| … of it in system calls | 14 / 19 / 107 | 4 / 7 / 9 |
| Signal handlers run | 3 / 3 / 4 | 1 / 1 / 2 |

`pylond`'s histograms for the run: handoff 96 / 112 / 768 µs, wake
36 / 52 / 704 µs, park 52 / 60 / 60 µs.

- **Exit to the first handler:** 5–8 µs; the 17 µs above was mostly
  the probes. With them, each step (`get_signal`, building the signal
  frame, the return to user space) took 1–4 µs.
- **A park's extra handlers:** `pylond` re-sends the kick 10, 20 and
  40 µs after the first signal (`REVOKE_RESIGNAL_US`) while the donor
  hasn't published `PARKED`. By then it has usually left `KVM_RUN` and
  is parking: 207 of 390 parks got two re-sends, 16–18 µs after the
  exit at the median, and each runs a handler. No single system call
  took over 4 µs at the median (`KVM_GET_MSRS`, `KVM_GET_REGS`,
  `KVM_SET_MP_STATE`, `timer_settime`).
- **A park's tail:** a donor that has published `PARKED` can be
  preempted by the new owner before its next `KVM_RUN`, and then waits
  about 1 ms. It is already parked, so the park latency doesn't show it.
- **The wake and handoff tail:** most signals that waited 0.2–3 ms
  for a CPU sent the vCPU to a CPU other than the one `pylond` gave it.
  `scx_pylon` kept a CPU's woken turn after the CPU changed owner, so a
  sibling of the new owner, woken to share its VM's old CPU, ran first
  and the new owner waited up to a tick. The new owner now takes the
  first turn ([#104](https://github.com/CMKL-APEX/pylon/pull/104)):
  wakes slower than 200 µs in the trace fell from 7–32 per run to 0–2.
- **The rest of `pylond`'s tail:** with that fixed, `pylond`'s wake
  p99 stayed at 0.2–1.2 ms. `pylond` records a wake when the VMM
  publishes `RUNNING`, after the `KVM_RUN` exit, the wake `eventfd`
  read and `KVM_SET_MP_STATE`. Measured that way in the trace, the slow
  wakes match the histograms. The new owner left the guest 12–20 µs
  after the signal, and then a woken sibling of its VM, queued to share
  the CPU, preempted it at once and ran guest code for a whole tick,
  about 1 ms: the 100 µs share slice only ended at the tick. `scx_pylon`
  now lets a turn run 100 µs before a woken vCPU takes the CPU, and a
  timer ends each shared turn after 100 µs
  ([#107](https://github.com/CMKL-APEX/pylon/pull/107)). In two runs
  each against `main`, wakes over 0.5 ms in the trace went from 7–9 to
  0, `pylond`'s wake p99 from 896–1,152 µs to 160 µs, its handoff p99
  from 960–1,152 µs to 208 µs, and the median turn from 1,000 µs to
  115 µs. The shared CPUs switch about 8 times as often (about 305,000
  switches per 20 s against 37,000). Separately, the engine lost its
  CPU to a vCPU thread for 20 ms, 1–2 times per trace, which
  [#105](https://github.com/CMKL-APEX/pylon/pull/105) fixes.

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
- Handoff latency without `scx_pylon`, and on an Intel host (above:
  AMD, with `scx_pylon`).
- The rest of a forced park's 45 µs
  ([#54](https://github.com/CMKL-APEX/pylon/issues/54)), and what 100 µs
  turns cost guest throughput on a shared CPU.
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
