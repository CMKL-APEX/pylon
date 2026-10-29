# Performance and benchmarks

This page collects Pylon's performance figures: what was run, where,
when and how, and how to run it again. There are few so far. Pylon has
**no measurement of a real core handoff between real VMs yet**: that is
step S6 of [Enhancements](enhancements.md#implementation-steps).

## How to read the numbers

- Every figure was taken once, on one date, on one machine. It is a data
  point, not a guarantee.
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
it. The reason hasn't been investigated: thread scheduling without
`scx_pylon` assignments for these worker threads, the engine sharing
CPUs with the workload, and the benchmark's own polling are all
candidates. The ACPI column is mostly the injected delays, so the ratio
between the columns follows from the model's assumptions. Don't quote it
as a measured speed-up.

## Other figures

| Figure | Kind | Source |
|---|---|---|
| Grace period: 8 µs | Default | `REVOKE_GRACE_PERIOD_US`, `pylond/src/engine.rs` |
| Pause between sweeps: 1,000 TSC cycles (~330 ns at 3 GHz) | Default | `DEFAULT_TPAUSE_CYCLES` |
| Handoff target: "~3–6 µs" | Design goal | `spec/00-architecture.md` §1 |

## Gaps

These haven't been measured and are needed before Pylon's latency can
be claimed:

- The engine's sweep time, by number of VMs.
- Kick to `PARKED`, and revocation to recipient running, between real
  VMs, with and without `scx_pylon` (step S6).
- Wake latency of a parked vCPU in a real VM.
- `scx_pylon`'s cost for non-VM tasks on the host.
- The same `pylon-bench` run on a CPU with `WAITPKG`.

When you add a figure, add it here with its date, hardware, build and
command, and say whether it was measured or modelled.

## Running the benchmark

```bash
cargo run --release -p pylond --bin pylon-bench            # the run above
cargo run --release -p pylond --bin pylon-bench -- --quick # short preset used by CI
cargo run --release -p pylond --bin pylon-bench -- --help  # all options
```
