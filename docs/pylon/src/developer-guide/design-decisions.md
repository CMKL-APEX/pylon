# Design decisions and trade-offs

This page explains why Pylon is built the way it is. For each choice a
contributor is likely to question, it gives the decision, the reason,
what it costs, and when to look at it again. The reasons come from the
specs ([`spec/00-architecture.md`](https://github.com/CMKL-APEX/pylon/blob/main/spec/00-architecture.md)
to [`06`](https://github.com/CMKL-APEX/pylon/blob/main/spec/06-guest-kick-path.md))
and from pull requests. Where the project hasn't written a reason down,
this page says so.

To change a decision, change the spec that records it first, in its own
pull request or as the first commit of one, then the code
([Contributing](contributing.md)).

## The goal: space-sharing, not time-sharing

**Decision.** A managed host CPU runs at most one vCPU at a time. The
host never time-slices two runnable vCPUs on one CPU. Instead, the
guest schedules its threads onto the vCPUs it currently has, and the
host moves whole CPUs between VMs (spec 00 §1, invariants 1 and 2).

**Why.** Time-slicing vCPUs gives the guest scheduling on two levels
that don't know about each other (the "double-scheduling tax"). It also
preempts vCPUs that hold guest locks, which stalls every other vCPU
spinning on that lock. Space-sharing removes both, if CPUs can be moved
fast enough to follow demand.

**Cost.** Every other decision on this page exists to make CPU moves
fast and safe. A guest that doesn't report demand gets no elasticity.

## Park vCPU threads instead of hotplugging vCPUs

**Decision.** All vCPUs exist and are online in the guest from boot.
Pylon "removes" one by blocking its host thread on an `eventfd`.

**Why.** ACPI CPU hotplug goes through firmware notification and the
guest's CPU online/offline path, which takes milliseconds per CPU (see
[Benchmarks](benchmarks.md) for the modelled comparison). Blocking and
waking a thread takes microseconds and needs nothing from the guest.

**Cost.** The guest doesn't know a vCPU is parked. Work it queues there,
and interrupts it sends there (TLB shootdowns, reschedule IPIs), wait
until `pylond` wakes the vCPU. Solving that needs guest cooperation:
spec 06 open question Q2.

**Revisit** if the guest side can't be made to treat parked vCPUs as
preempted.

## Shared memory on the hot path

**Decision.** Demand, revocation requests, vCPU state and CPU ownership
all pass through shared memory: the per-VM page and the BPF owner map.
The socket is used only for registration, policy and stats.

**Why.** A system call costs about a microsecond with today's
mitigations. A handoff budget of a few microseconds (spec 00 invariant
3: "~3–6 µs core handoff") leaves no room for several of them.

**Cost.** Every field needs an ownership and ordering rule, and the
ABI is versioned and hard to change ([Shared-memory ABI](shared-memory-abi.md)).
The rules weren't strict enough at first (spec 06 P6).

## A polling arbiter on a dedicated core

**Decision.** `pylond`'s engine spins on host core 1, sweeping every
VM's page about every microsecond, and waits between sweeps with
`TPAUSE` (spec 02 §1).

**Why.** Interrupt- or event-driven designs put a wakeup, which costs
microseconds, on the path of every decision. Polling sees a change
within one sweep.

**Cost.** One host core is permanently busy, and a second (core 0)
runs the control plane. On a small host that's a large share. Sweep
time grows with the number of VMs (up to 64 per sweep).

**Revisit** if hosts are small, or if `UMONITOR`/`UMWAIT` on the
telemetry lines can replace polling (the earlier `docs/pylon.md`
mentioned them; the code doesn't use them).

## Two planes joined by ArcSwap

**Decision.** Everything that can block (socket I/O, JSON, `mmap`, HTTP)
runs on Plane 1. Plane 2 sees the VM list only through
`ArcSwap<Vec<VmHandle>>` (spec 02 §1).

**Why.** It keeps the engine free of locks, allocation and blocking
without making Plane 1's code unusual.

**Cost.** Registering or removing a VM copies the whole list. That's
fine at tens of VMs per host.

## Cooperative revocation with a hard deadline

**Decision.** `pylond` asks a donor to yield and waits up to 8 µs
(`REVOKE_GRACE_PERIOD_US`). After that, it takes the CPU anyway.

**Why.** Asking lets the guest finish a critical section first, so the
revocation itself doesn't cause lock-holder preemption. The deadline
bounds how long a recipient can wait for an uncooperative or broken
guest. The project hasn't recorded why 8 µs specifically. It matches
the "~3–6 µs" handoff target plus margin.

**Cost.** It needs a working guest-side path to be cooperative at all,
and that path isn't built yet ([Enhancements](enhancements.md)).

## `sched_ext` for the kernel side

**Decision.** A BPF `sched_ext` scheduler runs each CPU's owner, reading
`pylond`'s decisions from a memory-mapped map (spec 03 §1).

**Why.** It's the only way to change scheduling decisions in a stock
kernel without a kernel module or patch, and the map makes each
decision a memory store.

**Cost.** `sched_ext` replaces the scheduler for the whole host while
it's attached, not only for vCPU threads. It needs Linux 6.12 or later.
`pylond` therefore treats it as optional: it attaches it when it can
and runs without it otherwise ([#9](https://github.com/CMKL-APEX/pylon/pull/9)).

## A fixed guest-physical address

**Decision.** The page is a RAM slot at `0xFEDF_0000`, not a device.

**Why.** It needs no device model, and a guest can find it with no
discovery protocol. The reason isn't recorded beyond simplicity.

**Cost.** The address isn't reserved in Cloud Hypervisor's layout. The
page has its own memory slot but isn't guest RAM, so it stays out of the
guest's E820 table (it used to be listed as RAM). Spec 06 adds a PCI
device that reports the address, and §9 plans to move the page into its
BAR.

## Isolate from upstream

**Decision.** New crates under `pylon/`, VMM logic in one new file, and
one-line hooks in upstream files (spec 00 §2, spec 04 §1).

**Why.** Upstream Cloud Hypervisor changes quickly. Every line Pylon
changes in an upstream file is a possible conflict at each merge.

**Cost.** The VMM side reaches its state through a process-global
instead of passing it explicitly, and VMM features that need more than a
hook (the `pylon-kick` device in spec 06) still need a small upstream
touch.

## JSON over a Unix socket, with SCM_RIGHTS

**Decision.** The control protocol is one JSON object per connection,
with file descriptors attached as `SCM_RIGHTS`.

**Why.** It's off the hot path, easy to drive from a shell or from
Glidex, and passing descriptors lets `pylond` map pages and signal
`eventfd`s without access to the VMM's files. The project hasn't
recorded why it chose JSON over a binary format; human readability is
the likely reason.

**Cost.** Message types are told apart by trial parsing
([Control socket protocol](../reference/control-socket.md)), and there
is no authentication beyond socket permissions.

## A `no_std` ABI crate

**Decision.** `pylon-abi` is `#![no_std]` with no dependencies.

**Why.** The same types must compile into a guest kernel module or a
Rust guest kernel (spec 00 names Asterinas `ostd`), not only into
userspace.

## A userspace guest agent first

**Decision.** The first guest component is a userspace daemon that
reads `/proc`.

**Why.** It works with an unmodified guest kernel and was quick to
build.

**Cost.** It reports demand milliseconds late, can't see critical
sections and can't act on revocations, and its attempt to act on them
is wrong (spec 06 P4). Spec 06 makes it telemetry-only and moves
cooperation into a kernel module.
