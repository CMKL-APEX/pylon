# The scx_pylon dispatcher

`scx_pylon` is a `sched_ext` scheduler: a set of BPF callbacks that the
kernel calls to decide where and when tasks run. It's in
[`pylon/scx-pylon`](https://github.com/CMKL-APEX/pylon/tree/main/pylon/scx-pylon),
and its design is in
[`spec/03-scx-and-vmm-hooks.md` §1](https://github.com/CMKL-APEX/pylon/blob/main/spec/03-scx-and-vmm-hooks.md#1-pylonscx-pylon-zero-syscall-sched_ext-dispatcher).

## Why it exists

`pylond` decides in microseconds that a CPU belongs to vCPU thread *T*.
Without `scx_pylon`, waking *T* only makes it runnable. The host's fair
scheduler then picks a CPU and a moment, with its own wakeup latency and
runqueue locks. `scx_pylon` makes the kernel act on `pylond`'s decision
directly, and `pylond` communicates that decision with memory stores,
not system calls.

## The map

```c
struct pcpu_owner_entry { __u32 target_tid; __u32 flags; };

struct {
    __uint(type, BPF_MAP_TYPE_ARRAY);
    __uint(max_entries, 256);              // indexed by host CPU
    __uint(map_flags, BPF_F_MMAPABLE);
    __type(key, __u32);
    __type(value, struct pcpu_owner_entry);
} pylon_pcpu_owners SEC(".maps");
```

`BPF_F_MMAPABLE` lets userspace `mmap` the array. `pylond` maps it and
uses it as its `PcpuOwnerTable`, so an owner change is two atomic
stores: `flags`, then `target_tid` with `Release`. `flags` is defined
(`ACTIVE`, `PREEMPT`) but `pylond` writes `0`, and the BPF program
doesn't read it.

## Callbacks

**`select_cpu(p, prev_cpu, wake_flags)`**, when a task wakes:

1. If `prev_cpu`'s owner is `p`, return `prev_cpu`.
2. Otherwise scan all 256 entries for one owned by `p`, and return that
   CPU if found.
3. Otherwise use `scx_bpf_select_cpu_dfl`, the default placement.

A task found as an owner is marked in task-local storage with its VMM
process's tgid: its *domain*. A vCPU thread gets its domain earlier, at
its first `enqueue`, when `pylond` has listed it in `pylon_vcpu_dom`.
`pylond`'s control plane keeps that map, thread ID to VMM tgid, in line
with the registered VMs' shared pages every second. `enqueue` checks the
tgid too, so a thread ID reused by another process isn't tagged. A vCPU
is therefore isolated whether or not it ever owns a CPU, which matters
because `pylond` only gives CPUs to vCPUs that start parked
(`initial_active_vcpus`).

**`enqueue(p, enq_flags)`** puts each task on one of three kinds of
queue:

- The owner of its CPU goes on that CPU's owner queue (DSQ id = CPU)
  with a 20 ms slice, and the CPU is kicked with `SCX_KICK_PREEMPT`.
  This is what preempts a donor whose CPU was given to a recipient.
- Another vCPU thread with a domain goes on the vCPU queue. If it owns
  a CPU elsewhere, that CPU is kicked.
- A host task allowed on only one CPU, such as a per-CPU kworker, goes
  on that CPU's local queue, and an idle CPU is kicked.
- Everything else goes on the host queue.

A task with migration disabled stays on its CPU's local queue too. The
kernel runs a CPU's local queue before it asks `dispatch` for work, so
when the owner's slice ends, a pinned task waiting there runs first. The
owner then gets the CPU back. With
`SCX_ENQ_LAST`, the CPU has already decided to idle with `p` runnable,
so `enqueue` kicks it to schedule again.

**`dispatch(cpu, prev)`**, when a CPU needs work, tries in order:

1. The CPU's owner queue. Tasks that no longer own the CPU are moved to
   the vCPU or host queue.
2. The vCPU queue, skipping threads that own another CPU.
3. The host queue.

Each step only dispatches what the SMT claims allow. If nothing fits,
the CPU idles.

## SMT isolation

Kernel core scheduling (`PR_SCHED_CORE`) can't be used with
`sched_ext` on kernels that lack the upstream fixes for
4c95380701f5 (issue #15). So `scx_pylon` does the job itself for managed
vCPU threads: while a CPU runs a vCPU of VM *A*, its SMT siblings run
only vCPUs of *A* or nothing.

Each CPU publishes a claim in `cpu_claim[]`: idle, host, pinned (a host
task allowed on only that CPU), or a domain (tgid).

- `dispatch` publishes the claim before checking the siblings. Two
  siblings claiming at once each see the other's claim.
- `running` records the task actually running, and `update_idle` clears
  the claim.
- The order is: a domain beats host, and between two domains the
  lower-numbered CPU wins. The winner kicks the loser with
  `SCX_KICK_PREEMPT`, and the loser's next `dispatch` backs off.
- A pinned task can't go anywhere else, so it runs without checking its
  siblings. Instead, a domain yields to it, and `running` kicks the
  siblings whose domain claims yield when a pinned task starts. The
  exception is a CPU's owner: it doesn't yield, or a busy pinned task
  next to it would keep it waiting until the watchdog fired.
- When a domain or pinned claim ends, the siblings are kicked to look
  for work again.

The siblings come from `pylon_smt_next`, a ring (next CPU on the same
core) that `load_and_attach()` fills from
`/sys/devices/system/cpu/cpu<N>/topology/thread_siblings_list` before
attaching. When `scx_pylon` is attached, `pylond` gives the engine the
same ring. It then never assigns two VMs sibling threads of one core,
and prefers the free thread of a core the VM already holds.

Not isolated:

- tasks of higher sched classes (RT, deadline, stop);
- interrupts and softirqs;
- tasks with migration disabled, which are short and pinned;
- host tasks allowed on only one CPU, mostly per-CPU kernel threads,
  when they run on an owned CPU or next to its owner. They can't wait
  for the owner to block: a busy vCPU may never block, and the kernel's
  watchdog would eject the scheduler.

Kernel core scheduling doesn't isolate interrupts either.

## Core scheduling guard

Kernel core scheduling turns on for the whole host as soon as any task
has a cookie, for example a stock Cloud Hypervisor VM, which asks for
one by default (`core_scheduling=vm`). On kernels without upstream
f2da9587118d and d954004205c1, `sched_ext` then corrupts the
core-scheduling pick, which ends in a NULL dereference in
`pick_next_task()` or a hard hang (issue #15). The guard keeps
`scx_pylon` from running while any cookie exists. It lives in
`src/coresched.rs` and two more BPF programs:

- `KernelCoreSched::detect()` decides whether the kernel needs it.
  `PR_SCHED_CORE_GET` fails with `EINVAL` without `CONFIG_SCHED_CORE`
  and with `ENODEV` without SMT. Otherwise the kernel has the fix if
  its BTF has `struct scx_rq` member `lock_drop_seq`, which
  f2da9587118d adds and d954004205c1 depends on.
- `pylon_task_prctl` (`lsm/task_prctl`) refuses every `PR_SCHED_CORE`
  command except `GET`. It reads its answer, `EPERM` or `ENODEV`, from
  the `pylon_guard` map. `task_prctl` programs run as modify-return, so
  returning 0 lets the `prctl` go on as usual.
- `pylon_sched_core_get` (`fentry/sched_core_get`) records the tgid of
  a task that gets a new cookie anyway. `dispatch` then calls
  `scx_bpf_exit()` once with that pid, and the kernel falls back to the
  fair scheduler.

`load_and_attach(Some(deny))` attaches the LSM program and the watcher
first, then checks every thread with `PR_SCHED_CORE_GET`, and only then
attaches the scheduler. Both programs are left out of the load without
the guard (and the LSM program with `CookieDenial::Off`), since kernels
without `CONFIG_SCHED_CORE` have no `sched_core_get()`.

The flags are `SCX_OPS_ENQ_LAST`, `SCX_OPS_ENQ_EXITING`,
`SCX_OPS_ENQ_MIGRATION_DISABLED` and `SCX_OPS_KEEP_BUILTIN_IDLE`.

### Costs and open points

- The scan in `select_cpu` is up to 256 map lookups for every wakeup of
  every task on the host that isn't on its owned CPU, including tasks
  that own nothing. A reverse map (thread ID to CPU) would make it one
  lookup.
- An owner gives up its CPU only to tasks that can run nowhere else,
  for at most one slice each (20 ms). A busy task pinned to an owned
  CPU therefore halves the owner's time. A task that can run on two
  CPUs, both owned or fenced, still waits until a vCPU blocks, and the
  stall watchdog can eject the scheduler.
- The program is attached for the whole host (`.name = "pylon"`, no
  partial-switch flag), so every `SCHED_NORMAL` task goes through it.
- Two VMs on sibling threads (only possible without `pylond`'s
  placement, for example with VMs it doesn't manage) take turns badly:
  the lower-numbered CPU's VM wins for as long as its vCPU runs.

None of these has been measured yet.

## Building

`build.rs` runs `clang -target bpf -O2 -g -Wall -Werror` on
`src/bpf/scx_pylon.bpf.c`, with `libbpf-sys`'s headers and the checked-in
`vmlinux.h`, and writes `scx_pylon.bpf.o` to `OUT_DIR`. The library
embeds that object with `include_bytes!`. Cross and musl builds can't
run this step, so CI excludes the crate there
([Contributing](contributing.md#ci)).

`vmlinux.h` holds the kernel's type definitions for BPF CO-RE and is
checked in. If the program needs types it doesn't have, regenerate it
on a `sched_ext` kernel with
`bpftool btf dump file /sys/kernel/btf/vmlinux format c`, and keep its
SPDX header.

## Loading

`ScxPylonUserspace` in `src/lib.rs` wraps `libbpf-rs`:

- `open_embedded()` opens the embedded object.
- `load_and_attach(guard)` loads it, fills `pylon_smt_next`, sets up
  the core scheduling guard if asked, attaches the `struct_ops` link,
  and maps `pylon_pcpu_owners`.
- `into_parts()` hands `pylond` the mapping (as its owner table), the
  link, the guard's links and the object. `pylond` keeps the link alive for its lifetime.
  The kernel detaches the scheduler when the link's descriptor closes,
  including when `pylond` dies.

`PcpuOwnerMap` is the userspace view of the map. It can also be backed
by a file or anonymous memory, which is how the tests run without a
`sched_ext` kernel.

## Tests

`cargo test -p scx-pylon` checks the embedded object (map name, type,
size and flags) and the `PcpuOwnerMap` operations. `tests/live_attach.rs`
attaches to the running kernel and is `#[ignore]`d because it needs
root.
