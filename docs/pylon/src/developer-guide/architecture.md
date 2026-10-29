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
| **VMM** (`vmm/src/pylon.rs`) | The shared-page file and its mapping; two `eventfd`s per vCPU; the vCPU threads. | The page (with the guest and `pylond`); the `eventfd`s (with `pylond`). |
| **`pylond` Plane 1** | The Unix socket; the list of registered VMs, published as an immutable `Arc<Vec<VmHandle>>` through `ArcSwap`. | Each VM's page, mapped from the file descriptor the VMM sent. |
| **`pylond` Plane 2** | All arbitration state: each managed CPU's `CoreState` (`Idle`, `Assigned`, `Revoking`), per-VM demand records, metering timestamps. None of it is shared or locked. | Writes the pCPU owner table and vCPU slots' host-side fields. |
| **`scx_pylon`** | Nothing; it only reads. | The owner table, as a memory-mapped BPF array map. |
| **Guest** | Its telemetry and its vCPU slots' guest-side fields. | The page. |

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
   opens) the `shm` file, sets it to 8,192 bytes, maps it, zeroes it,
   writes the header, and creates a kick and a wake `eventfd` for each
   vCPU slot.
2. **VMM → Plane 1.** If `socket` exists, `register_with_daemon` sends
   one message: newline-terminated JSON (`vm_id`, `num_vcpus`, `kick_signal`,
   `min_cores`, `max_cores`, `priority_weight`), with the file
   descriptors in `SCM_RIGHTS` in this order: the page, the kick
   `eventfd`s, the wake `eventfd`s.
3. **Plane 1.** Maps the page from the descriptor, checks the header,
   records the VMM's PID from `SO_PEERCRED` (with a pidfd, for signalling
   it later),
   builds a `VmHandle`, and publishes a new VM list without any old
   entry that has the same `vm_id`. It replies with
   `{"accepted":true,...}`. The VMM doesn't wait for the reply.
4. **VMM, memory setup.** `setup_memory_region` maps the same file into
   guest-physical memory at `gpa` as a shared memory slot.
5. **VMM, vCPU threads.** Each thread stores its `gettid()` in its slot's
   `host_tid`. Threads at or above `initial_active_vcpus` park on their
   wake `eventfd` before their first `KVM_RUN`.
6. **Plane 2.** Sees the VM in its next sweep.

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
   `state = RUNNING` and enters `KVM_RUN`.
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
   deadline ([phase 2](arbitration.md#phase-2-revocations-in-flight)).
4. **Kernel.** With `scx_pylon`, `dispatch` on that CPU sees a new owner
   and preempts the donor thread if it's still there.

> **Today, step 2 doesn't happen.** Nothing turns the kick into a guest
> interrupt, and no guest component writes `PARKING`, so every
> revocation ends with step 3's forced park. See
> [Enhancements](enhancements.md#problems-in-the-current-revocation-path).

## Flow: giving a core back

When a vCPU parks while no revocation is pending, the engine's first
phase sees, on an assigned CPU, `state = PARKED` with
`revoke_requested = 0` and a higher `yield_count` than at assignment.
It meters the time so far, clears the owner entry, sets the slot's
`assigned_pcpu` to unassigned, and marks the CPU `Idle`.

## Threads in a Pylon VMM

Pylon adds no threads to the VMM. It uses the existing vCPU threads:
they park and wake themselves. A parked vCPU thread is blocked in
`read(2)` on its wake `eventfd`, outside `KVM_RUN`, holding no locks.
