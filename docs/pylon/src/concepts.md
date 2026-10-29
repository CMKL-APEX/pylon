# Concepts

This page names the parts of Pylon and the words the rest of the book
uses for them.

## The parts

| Part | What it is | Where it runs |
|---|---|---|
| **Pylon VMM** | Cloud Hypervisor with the `--pylon` option. It creates the VM's shared page, maps it into the guest, registers the VM with `pylond`, and parks and wakes vCPU threads. | One process per VM, on the host. |
| **`pylond`** | The per-host *core arbiter*. It decides which vCPU runs on which managed host CPU. | One daemon per host, on host cores 0 and 1. |
| **`scx_pylon`** | A Linux `sched_ext` BPF scheduler. It runs the vCPU thread that `pylond` assigned to each CPU. Optional. | In the host kernel. |
| **Guest side** | Code in the guest that reports demand into the shared page. Today that's `pylon-guest-agent`, a userspace daemon. | Inside the VM. |
| **`pylon-abi`** | The `#![no_std]` Rust crate that defines the shared page's layout. Every other part uses it. | Compiled into all of the above. |

## The words

**Managed CPUs** are the host CPUs listed in `pylond --cores`. Only these
are given to VMs. `pylond` itself runs on host cores 0 and 1.

**Shared page.** An 8 KiB memory region per VM, backed by a file such as
`/dev/shm/vm1-pylon`. It's mapped into the VMM, into `pylond`, and into
the guest at a fixed guest-physical address (`0xFEDF_0000` by default).
It holds a header, guest telemetry, and one 64-byte slot per vCPU.

**Telemetry** is what the guest writes about its own demand: how many
threads are runnable, how long the oldest one has waited, and when its
next timer fires.

**vCPU states.** Each vCPU slot has a state:

| State | Meaning |
|---|---|
| `RUNNING` | The vCPU thread runs guest code. |
| `PARKED` | The vCPU thread is blocked in the VMM, waiting to be woken. It uses no CPU. |
| `WAKING` | `pylond` has given the vCPU a core and signalled it to wake. |
| `PARKING` | Defined for a guest that is about to park (see [Enhancements](developer-guide/enhancements.md)); not used yet. |

**Park and wake.** *Parking* a vCPU blocks its host thread on an
`eventfd` instead of running `KVM_RUN`. *Waking* it writes that
`eventfd`. To the guest, a parked vCPU looks like a vCPU that isn't
being scheduled.

**Assign.** `pylond` *assigns* a managed CPU to one vCPU of one VM. It
records the assignment in the vCPU's slot (`assigned_pcpu`) and in the
**pCPU owner table**, which maps each host CPU to the thread ID of the
vCPU that owns it. `scx_pylon` reads that table.

**Revoke.** To move a core from a *donor* VM to a *recipient* VM,
`pylond` sets the donor vCPU's `revoke_requested` flag and gives it a
**grace period** of 8 µs to park. A donor that parks in time has made a
**cooperative yield**. One that doesn't is **forcibly preempted** when
the deadline passes. See [Status and roadmap](status.md) for how much of
this path works today.

**Critical section.** The guest sets a vCPU's `in_critical_section`
while it holds a spinlock, handles an interrupt or has preemption
disabled. `pylond` prefers donors that aren't in one, because
preempting a lock holder stalls every other vCPU waiting for that lock.

**Policy.** Each VM has `min_cores`, `max_cores` and a
`priority_weight`. They are set with `--pylon` at boot and can be
changed at runtime over `pylond`'s control socket.

**Urgency.** `pylond`'s score for how badly a VM needs another core. It
grows with how long the guest's oldest runnable thread has waited,
multiplied by the VM's priority weight. A VM below its `min_cores` gets
a large boost, and a VM waking from zero cores gets the maximum.

**Scale to zero.** A VM with no cores at all. `pylond` wakes it when
the time in its `next_timer_tsc` arrives.

**Core-microseconds.** The metering unit: one managed CPU assigned to a
VM for one microsecond.
