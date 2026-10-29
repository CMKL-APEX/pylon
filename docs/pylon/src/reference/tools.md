# Other tools

## scx_pylon

Inspect or test the `sched_ext` scheduler without `pylond`. Built with
`cargo build --release -p scx-pylon`.

```text
scx_pylon [--verify-only] [--status] [--attach] [--shm <path>]
          [--core-sched-guard <auto|on|off>] [--core-sched-deny <eperm|enodev|off>]
```

| Option | Meaning |
|---|---|
| `--verify-only` | Check the embedded BPF object (the map's name, type, size and flags) and exit. Needs no privileges. |
| `--status` | Print the kernel's `sched_ext` state (`state`, `enable_seq`, `nr_rejected`) and exit. With `--shm`, also print the owner table from that file. |
| `--shm <path>` | A file-backed owner table, for `--status`. |
| `--attach`, or no option | Attach the scheduler and hold it until `SIGINT` or `SIGTERM`. Nothing writes its owner table, so this is for testing the BPF program only. Needs root, and fails if another `sched_ext` scheduler (including `pylond`'s) is attached. |
| `--core-sched-guard`, `--core-sched-deny` | As for [`pylond`](pylond.md). |

## pylon-guest-agent

Runs inside a Linux guest and reports demand into the shared page
([Inside the guest](../user-guide/guest.md)). Built with
`cargo build --release -p pylon-guest-agent`.

```text
pylon-guest-agent [--gpa <addr>] [--shm <path>] [--interval-us <n>] [--status] [--once]
```

| Option | Default | Meaning |
|---|---|---|
| `--gpa <addr>` | `0xFEDF0000` | Guest-physical address of the page, mapped through `/dev/mem`. |
| `--shm <path>` | none | Map this file instead of `/dev/mem`. For testing on the host. |
| `--interval-us <n>` | `50000` | Time between updates, in microseconds. |
| `--status` | | Print the header and every vCPU slot, and exit. |
| `--once` | | Do one update, print the status, and exit. |

It logs through `env_logger` (`RUST_LOG`, default `info`) and exits on
`SIGINT` or `SIGTERM`.

## pylon-bench

The Pylon vs. ACPI hotplug model benchmark
([Performance and benchmarks](../developer-guide/benchmarks.md)). Built
with the `pylond` package.

```text
pylon-bench [--waves <n>] [--max-vcpus <n>] [--tasks-per-wave <n>] [--iters-per-task <n>]
            [--acpi-bringup-us <n>] [--acpi-eject-us <n>] [--quick]
```

| Option | Default | Meaning |
|---|---|---|
| `--waves` | 20 | Burst waves (scale out from 1 to `max-vcpus`, then back in). |
| `--max-vcpus` | 8 | Workers at the top of each wave. |
| `--tasks-per-wave` | 64 | Compute tasks per wave. |
| `--iters-per-task` | 25000 | Work per task. |
| `--acpi-bringup-us` | 2500 | Modelled guest CPU online time per hotplugged CPU. |
| `--acpi-eject-us` | 1500 | Modelled guest CPU offline time per removed CPU. |
| `--quick` | | A short preset for CI. |
