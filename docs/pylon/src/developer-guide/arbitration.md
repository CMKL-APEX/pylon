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
revocation pending, the vCPU gave the core up by itself. The engine
meters the time since the last sample, clears the CPU's owner entry,
sets the slot's `assigned_pcpu` to unassigned, and marks the CPU `Idle`.

**Meter.** For each `Assigned` or `Revoking` CPU, the engine adds the
elapsed time to the owning (or donating) VM's `allocated_core_usec`:

```text
usec = (now_tsc - last_tsc) * 1000 / tsc_khz
```

It carries the remainder forward (it advances `last_tsc` by exactly the
cycles it counted), so rounding doesn't lose time across sweeps.
`tsc_khz` comes from the VM's header, with 3,000,000 as the default.

## Phase 1: demand

For each VM, the engine counts its **assigned** cores (CPUs it holds)
and **incoming** cores (CPUs being revoked for it), and reads its
telemetry.

**Zero-core wakeup.** A VM with no committed cores qualifies if its
`next_timer_tsc` has passed, or if the host's receive doorbell is ahead
of the guest's `rx_ring_cons_idx`.

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

For each `Revoking` CPU, the engine checks the donor's slot:

- **Donor `PARKED`:** a *cooperative yield*. It's counted on the donor VM.
- **Deadline passed:** a *forced preemption*. The engine increments the
  slot's `forced_preempt_count`, writes `state = PARKED` itself, and
  counts it on the donor VM.
- **Neither:** wait for the next sweep.

When the revocation finishes either way, the engine clears
`revoke_requested`, unassigns the donor, writes the recipient's thread
ID to the CPU's owner entry, sets the recipient slot's `assigned_pcpu`
and `state = WAKING`, writes its wake `eventfd`, and marks the CPU
`Assigned` to the recipient.

Writing `PARKED` for a donor that hasn't parked is one of the known
problems ([Enhancements](enhancements.md#problems-in-the-current-revocation-path)).

## Phase 3: matching

The engine sorts the demand records by urgency, highest first (an
in-place insertion sort over at most 64 entries). Then, for each VM in
that order, while it needs more cores (desired minus committed):

1. **Pick a vCPU.** `select_parked_vcpu` takes the lowest set bit of
   `stranded_work_bitmap` whose vCPU is parked (clearing that bit), or
   else the lowest-numbered parked vCPU. If the VM has none parked,
   move on to the next VM.
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
marks the CPU `Revoking`. The recipient's incoming count goes up, and the
donor's assigned count goes down, so later decisions in the same sweep
see the change.

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
| `forced_preempt_count` | `VmHandle` and the slot | Phase 2 |

Plane 1 reads them for the [stats request](../reference/control-socket.md#stats)
and the [metrics](../reference/metrics.md).

## Testing the engine

`poll_once(now_tsc)` takes the time as an argument, so tests drive it
with synthetic TSC values instead of real time. `pylond/tests/e2e_pylon.rs`
builds shared pages in temporary files, registers them through a real
control plane, and steps the engine to check wakeups, revocations,
deadlines, policy updates and metering.
