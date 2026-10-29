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

1. If `prev_cpu`'s owner is `p`, insert `p` into that CPU's local
   dispatch queue with an infinite slice and return `prev_cpu`.
2. Otherwise scan all 256 entries for one owned by `p`. If found, insert
   `p` there the same way, and kick that CPU with `SCX_KICK_PREEMPT` if
   it isn't `prev_cpu`.
3. Otherwise use `scx_bpf_select_cpu_dfl`, the default placement.

**`dispatch(cpu, prev)`**, when a CPU needs work: if the CPU has an
owner and `prev` isn't it, kick the CPU with `SCX_KICK_PREEMPT`. Then
take a reference to the owner task with `bpf_task_from_pid`, insert it
into the local queue with an infinite slice, and release the reference.

`init` and `exit` do nothing. There's no `enqueue`, so `sched_ext`'s
default behavior queues every other task.

### Costs and open points

- The scan in `select_cpu` is up to 256 map lookups for every wakeup of
  every task on the host that isn't on its owned CPU, including tasks
  that own nothing. A reverse map (thread ID to CPU) would make it one
  lookup.
- An infinite slice means an owner runs until it blocks. Other tasks
  queued on that CPU wait, and the kernel's stall watchdog can eject
  the scheduler if they wait too long.
- The program is attached for the whole host (`.name = "pylon"`, no
  partial-switch flag), so every `SCHED_NORMAL` task goes through it.

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
- `load_and_attach()` loads it, attaches the `struct_ops` link, and maps
  `pylon_pcpu_owners`.
- `into_parts()` hands `pylond` the mapping (as its owner table), the
  link and the object. `pylond` keeps the link alive for its lifetime.
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
