# The arbitration engine

`pylond`'s Plane 2, `MatrixEngine` in
[`pylon/pylond/src/engine.rs`](https://github.com/CMKL-APEX/pylon/blob/main/pylon/pylond/src/engine.rs),
decides which vCPU owns each managed CPU. This page walks through one
sweep. The original design is in
[`spec/02-pylond-arbiter.md`](https://github.com/CMKL-APEX/pylon/blob/main/spec/02-pylond-arbiter.md);
where the code differs, this page follows the code.

## The loop

```rust
// MatrixEngine::run, simplified
pin_current_thread_to_core(1);
while !stop {
    let now_tsc = read_tsc();
    self.poll_once(now_tsc);
    pause_cycles(now_tsc, DEFAULT_TPAUSE_CYCLES, self.has_waitpkg); // 1,000 cycles
}
```

`pause_cycles` uses `TPAUSE` until `now_tsc + 1000` on CPUs with
`WAITPKG`, and `PAUSE` otherwise. A thousand cycles is about 330 ns on a
3 GHz TSC, so with the sweep itself, a pass takes on the order of a
microsecond. That figure hasn't been measured; see
[Benchmarks](benchmarks.md#gaps).

### Invariants of a sweep

- **No heap allocation.** Per-VM demand records live in a fixed array of
  `MAX_TRACKED_VMS` = 64 entries; per-CPU state in arrays of `MAX_PCPUS`.
  At most 64 VMs are considered per sweep, in list order.
- **No locks and no blocking calls.** The VM list is loaded with one
  `ArcSwap::load`. The only system calls are `eventfd` writes to wake or
  kick a vCPU.
- **State is local.** `core_states` (`Idle`, `Assigned { vm_id, vcpu }`,
  `Revoking { donor…, recipient…, deadline_tsc }`) is owned by the
  engine thread alone.

## Phase 0: reclaim and meter

**Reclaim.** For each `Assigned` CPU whose vCPU is `PARKED` with no
revocation pending, and whose `yield_count` has gone up since the core
was assigned, the vCPU gave the core up by itself. The engine meters
the time since the last sample, clears the CPU's owner entry, sets the
slot's `assigned_pcpu` to unassigned, and marks the CPU `Idle`.

The `yield_count` check matters because `pylond` doesn't write `state`:
a vCPU that has just been given a core stays `PARKED` until its thread
wakes. The VMM increments `yield_count` before it publishes `PARKED`, so
only a vCPU that has run and parked again is reclaimed.

**Meter.** For each `Assigned` or `Revoking` CPU, the engine adds the
elapsed time to the owning (or donating) VM's `allocated_core_usec`:

```text
usec = (now_tsc - last_tsc) * 1000 / tsc_khz
```

It carries the remainder forward (it advances `last_tsc` by exactly the
cycles it counted), so rounding doesn't lose time across sweeps.
`tsc_khz` comes from the VM's header when it is non-zero. The VMM
writes `0`, so in practice it is the host TSC rate that `pylond`
measures at start (`calibrate_tsc_khz`, 50 ms against the system
clock), or 3,000,000 if that fails.

**Removed VMs.** A CPU still `Assigned` to a VM that is no longer
registered (Plane 1 removes VMs whose VMM exited) is freed: the engine
clears its owner entry and marks it `Idle`. This runs even when no VMs
are left.

## Phase 1: demand

For each VM, the engine counts its **assigned** cores (CPUs it holds)
and **incoming** cores (CPUs being revoked for it), and reads its
telemetry.

**Zero-core wakeup.** A VM with no committed cores qualifies if its
`next_timer_tsc` has passed, if a free parked vCPU's
`park_timer_deadline_tsc` has passed, or if the host's receive doorbell
is ahead of the guest's `rx_ring_cons_idx`.

**Time bases.** The guest writes `head_wait_tsc` and `next_timer_tsc`
on its own TSC. `pylond` converts them to host TSC with
`PylonSharedPage::guest_to_host_tsc`, which subtracts the offset the
VMM measured for vCPU 0. Everything else is already host TSC.

**Desired cores:**

| Case | Desired cores |
|---|---|
| Zero-core wakeup | `max(runnable, 1)` clamped to `[max(min_cores, 1), max_cores]` |
| `runnable_threads > 0` | `runnable_threads` clamped to `[min_cores, max_cores]` |
| Otherwise | 0 |

**Urgency** orders VMs in phase 3. `head_wait` is `now_tsc -
head_wait_tsc` (0 if the guest reports no waiting thread):

| Case | Urgency |
|---|---|
| Zero-core wakeup | `u64::MAX` |
| Wants more cores than committed | `max(head_wait, 1) × priority_weight`, plus 2⁶⁰ if below `min_cores` |
| Otherwise | `head_wait × priority_weight` |

So a VM below its minimum outranks every VM above it, and among the
rest, priority weight scales how long a thread has waited.

## Phase 2: revocations in flight

For each `Revoking` CPU, the engine checks the donor's slot. Only the
VMM writes `PARKED`, once the donor's thread has left `KVM_RUN`, so
`PARKED` means the donor has really stopped.

| Donor | Time | Action |
|---|---|---|
| `PARKED`, never signalled | any | A *cooperative yield*. Hand the CPU over. |
| `PARKED`, signalled | any | The VMM force-parked it. Hand the CPU over (already counted as forced). |
| not parked | before the deadline | Wait. |
| not parked | deadline passed | A *forced preemption*, counted once. Send the VMM's `kick_signal` to the donor's thread with `tgkill`, and again every `REVOKE_RESIGNAL_US` (10 µs). `KVM_RUN` returns `EINTR` and the VMM's post-exit hook force-parks the vCPU. |
| not parked | `FORCE_HANDOFF_US` (50 µs) after the deadline | Hand the CPU over anyway, counted as a *handoff without park*. `scx_pylon` preempts the donor thread, which parks at its next exit. |

The signal is resent because one that arrives while the vCPU thread is
outside `KVM_RUN` (handling an exit) doesn't stop the next guest entry.
`pylond` signals only a VMM that registered a `kick_signal`, and checks
through a pidfd that the VMM process still exists before each
`tgkill`.

Handing the CPU over unassigns the donor (`assigned_pcpu`), writes the
recipient's thread ID to the CPU's owner entry, sets the recipient
slot's `assigned_pcpu`, writes its wake `eventfd`, and marks the CPU
`Assigned` to the recipient. When the donor parked, the time from the
revocation request to `PARKED` goes into the VM's park latency
histogram. The engine doesn't touch the donor's `state` or
`revoke_requested`; the VMM clears `revoke_requested` when it parks.

If either VM has gone, the engine drops the revocation, marks the CPU
`Idle`, and releases the recipient's reservation.

## Phase 3: matching

The engine sorts the demand records by urgency, highest first (an
in-place insertion sort over at most 64 entries). Then, for each VM in
that order, while it needs more cores (desired minus committed):

1. **Pick a vCPU.** `select_parked_vcpu` takes the lowest set bit of
   `stranded_work_bitmap` whose vCPU is free (clearing that bit), else
   the lowest-numbered free vCPU whose parked timer is due, else the
   lowest-numbered free vCPU. A vCPU is *free* when it is
   `PARKED` and its `assigned_pcpu` is unassigned; a vCPU with a CPU on
   its way is still `PARKED` but already assigned or reserved. If the VM
   has no free vCPU, move on to the next VM.
2. **Fast path.** If a managed CPU is `Idle`, assign it (as in phase 2's
   hand-over) and continue.
3. **Preemption path.** Otherwise choose a donor with
   `select_donor_pcpu`, or stop matching for this VM if there's none.

### Donor selection

A CPU `Assigned` to another VM is an eligible donor if:

```text
(donor above its min_cores
   and (donor holds more than it desires
        or donor holds more than recipient's committed cores + 1
        or donor's urgency < recipient's urgency))
or (recipient is a zero-core wakeup and donor's urgency < recipient's urgency)
```

Among eligible donors, the engine prefers, in order:

1. a vCPU that isn't in a critical section;
2. a VM holding more than it desires;
3. the lowest urgency.

It then sets the donor slot's `revoke_deadline_tsc` to now plus
`REVOKE_GRACE_PERIOD_US` (8 µs, converted with the donor's `tsc_khz`),
stores `revoke_requested = 1`, writes the donor's kick `eventfd`, and
marks the CPU `Revoking`. It reserves the recipient vCPU by setting its
`assigned_pcpu` to that CPU, so no other match picks it. The recipient's
incoming count goes up, and the donor's assigned count goes down, so
later decisions in the same sweep see the change.

## The owner table

`PcpuOwnerTable` is an array of `{ target_tid: u32, flags: u32 }`
indexed by host CPU. It lives in one of three places:

| Backing | When |
|---|---|
| The `scx_pylon` BPF map, memory-mapped | `scx_pylon` is attached |
| A file (`--pcpu-map-file`) | For tests and tools |
| Anonymous memory | Otherwise. Nothing reads it. |

The engine writes it with plain atomic stores. No system call is needed
even when the kernel reads it.

## Metering and counters

| Counter | Where | Updated in |
|---|---|---|
| `allocated_core_usec` | `VmHandle` | Phase 0 |
| `cooperative_yield_count` | `VmHandle` | Phase 2 |
| `forced_preempt_count` | `VmHandle` | Phase 2, when a deadline passes |
| `handoff_without_park_count` | `VmHandle` | Phase 2 |
| `park_latency` (histogram, µs) | `VmHandle` | Phase 2, when a donor parks |

The slot's own `forced_preempt_count` belongs to the VMM, which
increments it when it force-parks a vCPU.

Plane 1 reads them for the [stats request](../reference/control-socket.md#stats)
and the [metrics](../reference/metrics.md).

## Testing the engine

`poll_once(now_tsc)` takes the time as an argument, so tests drive it
with synthetic TSC values instead of real time. `pylond/tests/e2e_pylon.rs`
builds shared pages in temporary files, registers them through a real
control plane, and steps the engine to check wakeups, revocations,
deadlines, policy updates and metering.
