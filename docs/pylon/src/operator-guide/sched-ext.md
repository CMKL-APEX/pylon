# Kernel scheduling with scx_pylon

`scx_pylon` is a Linux `sched_ext` scheduler, written in BPF and built
into `pylond`. When it's attached, the kernel runs each managed CPU's
assigned vCPU thread as soon as that thread is runnable, without going
through the host's fair scheduler.

## What it does

`pylond` and `scx_pylon` share one table: for each host CPU, the thread
ID of the vCPU that owns it. `pylond` writes the table through a
memory-mapped BPF map, with no system calls. The scheduler reads it:

- **When a vCPU thread wakes**, `scx_pylon` looks up the CPU that thread
  owns and queues it there with an unlimited time slice. If the thread
  was last on another CPU, it kicks the owned CPU so the switch happens
  at once.
- **When a CPU needs its next task**, `scx_pylon` dispatches the
  CPU's owner, and preempts the task on that CPU if it isn't the owner.
- **Every other task** is placed by `sched_ext`'s default CPU selection.

Without `scx_pylon`, the same table exists in `pylond`'s memory but
nothing reads it. Parked vCPU threads are still woken and blocked
through `eventfd`s, and the host's fair scheduler decides where and
when they run.

## When pylond attaches it

| Options | `scx_pylon` |
|---|---|
| none | Attached if `/sys/kernel/sched_ext` exists (the kernel was built with `CONFIG_SCHED_CLASS_EXT`). If attaching fails, `pylond` logs a warning and continues without it. |
| `--enable-scx` | Attached, or `pylond` exits with an error. |
| `--no-scx` | Never attached. |
| `--pcpu-map-file <path>` | Never attached; the table is kept in that file instead. (`--enable-scx` overrides this.) |

`pylond` logs `scx_pylon attached successfully; kernel sched_ext is
active` when it attaches. To check from the shell:

```bash
cat /sys/kernel/sched_ext/state          # "enabled" while attached
cat /sys/kernel/sched_ext/root/ops       # "pylon"
```

## What it means for the host

> **`scx_pylon` schedules every normal task on the host, not only VMs,**
> for as long as it's attached. That includes host services, VMM device
> threads and your shell. It detaches when `pylond` exits, including
> when `pylond` crashes, and the host returns to its normal scheduler.

Before attaching it on a shared host:

- An owned CPU runs its vCPU thread with an unlimited slice. Other
  tasks on that CPU wait until the vCPU blocks. Keep host work off the
  managed CPUs.
- The kernel watches `sched_ext` schedulers. If a runnable task is
  starved for too long (30 s by default), the kernel ejects the
  scheduler and logs why in `dmesg`. The host then runs on its normal
  scheduler while `pylond` keeps running without `scx_pylon`.
- Only one `sched_ext` scheduler can be attached at a time. If another
  is attached, attaching `scx_pylon` fails.

Use `--no-scx` when the host runs other latency-sensitive work you
don't want under a VM-oriented scheduler, or until you have tested
`scx_pylon` with your workload.

## The standalone scx_pylon tool

The `scx_pylon` binary (`cargo build --release -p scx-pylon`) can check
the embedded BPF object and show the kernel's `sched_ext` state without
`pylond`:

```bash
scx_pylon --verify-only   # check the embedded BPF object and exit
scx_pylon --status        # print the kernel's sched_ext state and exit
```

Run with no options (or `--attach`), `scx_pylon` attaches the scheduler
itself and holds it until `Ctrl-C`, with an owner table that nothing
writes. That's for testing the BPF program only. Don't run it next to
`pylond`.

See [Other tools](../reference/tools.md#scx_pylon) for all its options.
