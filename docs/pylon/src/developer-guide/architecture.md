# Architecture

This page describes Pylon's processes, what each one owns, and how the
two important flows move between them: registering a VM, and handing a
core from one VM to another. For the shared page's fields, see
[Shared-memory ABI](shared-memory-abi.md). For the engine's decisions,
see [The arbitration engine](arbitration.md).

## Processes and what they own

```text
 cloud-hypervisor --pylon (per VM)          pylond (per host)
 +------------------------------+           +----------------------------+
 | PylonVmmState                |  socket + | Plane 1 (core 0)           |
 |   shm file          ---------+--SCM_RIGHTS->  registrations, policy, |
 |   kick eventfd x N  ---------+---------->|  stats, metrics            |
 |   wake eventfd x N  ---------+---------->|    | publishes VM list     |
 |                              |           |    v (ArcSwap)            |
 | vCPU threads  <---- wake ----+-----------+ Plane 2 (core 1)           |
 |   KVM_RUN or parked          |           |  MatrixEngine, ~1 us sweep |
 +--------------+---------------+           +-------------+--------------+
                | maps the page at                        | writes owner
                | GPA 0xFEDF_0000                         | thread IDs
                v                                         v
 +------------------------------+           +----------------------------+
 | guest                        |           | pylon_pcpu_owners          |
 |   shared page (8 KiB) <------+-- mmap ---+ (BPF_F_MMAPABLE map)       |
 |   telemetry from the agent   |  (pylond) |   read by scx_pylon        |
 +------------------------------+           +----------------------------+
```

| Process | Owns | Shares |
|---|---|---|
| **VMM** (`vmm/src/pylon.rs`) | The shared-page file and its mapping; two `eventfd`s per vCPU; the vCPU threads; the `pylon-kick` PCI device (`vmm/src/pylon_kick.rs`), which turns each kick `eventfd` into an MSI-X interrupt for the guest. | The page (with the guest and `pylond`); the `eventfd`s (with `pylond`). |
| **`pylond` Plane 1** | The Unix socket; the list of registered VMs, published as an immutable `Arc<Vec<VmHandle>>` through `ArcSwap`. | Each VM's page, mapped from the file descriptor the VMM sent. Writes `scx_pylon`'s `pylon_vcpu_dom` map. |
| **`pylond` Plane 2** | All arbitration state: each managed CPU's `CoreState` (`Idle`, `Assigned`, `Revoking`), per-VM demand records, metering timestamps. None of it is shared or locked. | Writes the pCPU owner table, vCPU slots' host-side fields, and the `parked` masks in `pylon_vm_vcpus`; reads the `woken` masks. |
| **`scx_pylon`** | Its run queues and per-CPU claims. | Reads the owner table, `pylon_vcpu_dom` and the `parked` masks; writes the `woken` masks in `pylon_vm_vcpus`. The owner table and `pylon_vm_vcpus` are memory-mapped BPF array maps. |
| **Guest** | Its telemetry and its vCPU slots' guest-side fields, written by `pylon-guest-agent` and the `pv-pylon` module. | The page. |

### Why two planes

The engine has to react within microseconds, so its loop must never
block, allocate or take a lock that another thread can hold. Everything
that can do those things (socket I/O, JSON, `mmap`, HTTP) runs on
Plane 1. The two meet in one place: Plane 1 builds a new VM list and
swaps it in with `ArcSwap::store`. Plane 2 loads the current list at the
start of each sweep, which costs an atomic load and a reference count.
Policy changes are the one exception: Plane 1 writes them straight into
`VmHandle`'s atomic fields, and Plane 2 picks them up on its next read.

## Flow: registering a VM

1. **VMM, while parsing options.** `PylonVmmState::new` creates (or
   opens) the `shm` file, sets it to 16,384 bytes (the shared page, then
   the host page), maps it, zeroes it, writes both headers, and creates a kick and a wake `eventfd` for each
   vCPU slot.
2. **VMM → Plane 1, at memory setup.** `setup_memory_region` maps the
   same file into guest-physical memory at `gpa` as a shared memory
   slot. Then, if `socket` exists, `register_with_daemon` sends
   one message: newline-terminated JSON (`vm_id`, `num_vcpus`, `kick_signal`,
   `min_cores`, `max_cores`, `priority_weight`, and `wake_by_signal`
   when vCPUs park inside KVM), with the file
   descriptors in `SCM_RIGHTS` in this order: the page, the kick
   `eventfd`s, the wake `eventfd`s.
3. **Plane 1.** Maps the page from the descriptor, checks the header,
   records the VMM's PID from `SO_PEERCRED` (with a pidfd, for signalling
   it later),
   builds a `VmHandle`, and publishes a new VM list without any old
   entry that has the same `vm_id`. It replies with
   `{"accepted":true,...}` and closes the connection. The VMM waits up to
   1 second for the reply and logs a rejection as a warning.
4. **VMM, vCPU threads.** Each thread stores its `gettid()` in its slot's
   `host_tid`. The VMM tags vCPUs at or above `initial_active_vcpus`
   `park_on_boot`, and leaves them running so the guest can bring them
   online.
5. **Plane 2.** Sees the VM in its next sweep.
6. **Plane 1, once a second.** Lists the VM's vCPU threads in
   `pylon_vcpu_dom`. Once the guest agent has reported
   (`guest_heartbeat` is non-zero), it sets `revoke_requested` on each
   tagged vCPU that isn't `PARKED` yet and signals its thread, and the
   VMM parks it at its next exit.

## Flow: handing a core to a VM

When a managed CPU is idle (the fast path):

1. **Guest.** Reports demand: `runnable_threads` above the vCPUs it has,
   and `head_wait_tsc`.
2. **Plane 2, demand phase.** Computes the VM's desired cores and
   urgency.
3. **Plane 2, matching phase.** Picks a parked vCPU of that VM (vCPUs
   with stranded work first), and an idle managed CPU. It writes the
   vCPU's thread ID to that CPU's owner entry, sets the slot's
   `assigned_pcpu`, and writes the vCPU's wake
   `eventfd`.
4. **VMM.** The parked thread's blocking read returns. It sets
   `state = RUNNING` and enters `KVM_RUN`. A vCPU parked inside KVM
   (below) can't see the `eventfd` while it sleeps there, so for such a
   VM the engine also sends `kick_signal`; at the exit, the VMM finds
   the `eventfd` written and sets `state = RUNNING`.
5. **Kernel.** With `scx_pylon` attached, `select_cpu` finds the thread
   in the owner table and `enqueue` queues it on the owned CPU with a
   20 ms slice, renewed unless a task pinned to that CPU is waiting. That CPU's SMT siblings then run only vCPUs of the
   same VM or nothing. Without it, the fair scheduler places the thread.

When no managed CPU is idle, the engine takes one from a donor:

1. **Plane 2, matching phase.** Picks a donor vCPU on another VM
   ([donor selection](arbitration.md#phase-3-matching)), sets its
   `revoke_deadline_tsc` to now + 8 µs and `revoke_requested = 1`,
   writes its kick `eventfd`, reserves the recipient vCPU, and marks the
   CPU `Revoking`.
2. **Donor.** Is meant to notice, agree by writing `PARKING`, and exit
   to the VMM, which parks it and writes `PARKED`.
3. **Plane 2, revocation phase.** Once the donor is `PARKED`, moves the
   CPU to the recipient as in the fast path. If the deadline passes
   first, it signals the donor's thread with the VMM's `kick_signal`
   until the VMM force-parks it, and gives up waiting 50 µs after the
   first signal ([phase 2](arbitration.md#phase-2-revocations-in-flight)).
4. **Kernel.** With `scx_pylon`, the recipient's `enqueue` finds it
   owns the CPU, queues it there and kicks the CPU with
   `SCX_KICK_PREEMPT`. The kick preempts the donor thread if it still
   runs there, and `dispatch` runs the recipient.

> Step 2 needs the `pv-pylon` module in the guest. The kick `eventfd`
> raises the vCPU's MSI-X vector on the `pylon-kick` device; the
> module's per-CPU park thread writes `PARKING` and rings the park
> doorbell, which exits to the VMM. Without the module, every
> revocation ends with step 3's forced park. See
> [Inside the guest](../user-guide/guest.md#parking-cooperatively-the-pv-pylon-module).

## Flow: giving a core back

When a vCPU parks while no revocation is pending, the engine's first
phase sees, on an assigned CPU, `state = PARKED` with
`revoke_requested = 0` and a higher `yield_count` than at assignment.
It meters the time so far, clears the owner entry, sets the slot's
`assigned_pcpu` to unassigned, and marks the CPU `Idle`.

## Parking inside KVM

With `--pylon park=kvm`, the default on x86_64 KVM hosts, a vCPU parks
inside KVM instead of in the VMM
([spec 08](https://github.com/CMKL-APEX/pylon/blob/main/spec/08-park-in-kvm.md)).
The VMM halts it and returns to `KVM_RUN`, which sleeps until an
interrupt or a signal. An interrupt, such as the guest's timer, runs the
guest again without an exit and without a core: the vCPU stays
`PARKED`. `scx_pylon` marks such a vCPU *woken* and runs it on a CPU its
VM owns, in turns with that CPU's owner. `pylond` reads the woken
vCPUs, and counts one woken for 4 ms or longer as demand for a core of
its own. After a
park the guest or a revocation asked for, the VMM also makes the vCPU
runnable, still `PARKED`, once it has been parked for 4 ms
(`PARK_IN_KVM_BOUND`). The engine's side is in
[The arbitration engine](arbitration.md#vms-that-park-inside-kvm), and
`scx_pylon`'s in [The scx_pylon dispatcher](scx-pylon.md#callbacks).

## Threads in a Pylon VMM

Pylon adds no threads to the VMM. It uses the existing vCPU threads:
they park and wake themselves. A parked vCPU thread is blocked in
`read(2)` on its wake `eventfd`, outside `KVM_RUN`, holding no locks.
Inside KVM, it is instead halted in `KVM_RUN`, with a per-thread POSIX
timer that sends `SIGRTMIN` when the 4 ms bound passes.
