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
pin_current_thread_to_core(self.engine_cpu); // --engine-cpu, default 1
let sweep_cycles = ns_to_cycles(SWEEP_INTERVAL_NS, self.host_tsc_khz); // 333 ns
while !stop {
    let now_tsc = read_tsc();
    self.poll_once(now_tsc);
    pause_cycles(now_tsc, sweep_cycles, self.has_timed_wait);
}
```

`read_tsc` reads the host counter: the TSC on x86_64, `CNTVCT_EL0` on
aarch64 (`pylon_abi::clock`). `pause_cycles` waits until
`now_tsc + sweep_cycles` with `TPAUSE` on x86_64 CPUs with `WAITPKG` and
with `WFET` on aarch64 CPUs with `FEAT_WFxT`, and with `PAUSE` or a spin
hint otherwise; the wait is a hint and may end early. The interval is
333 ns, so with the sweep itself a pass takes on the order of a
microsecond. That figure hasn't been measured; see
[Benchmarks](benchmarks.md#gaps).

### Invariants of a sweep

- **No heap allocation.** Per-VM demand records live in a fixed array of
  `MAX_TRACKED_VMS` = 64 entries; per-CPU state in arrays of `MAX_PCPUS`.
  At most 64 VMs are considered per sweep, in list order.
- **No locks and no blocking calls.** The VM list is loaded with one
  `ArcSwap::load`. The only system calls are `eventfd` writes to wake or
  kick a vCPU, and `tgkill` to signal a vCPU thread (after a
  `pidfd_send_signal` check that the VMM still exists).
- **State is local.** `core_states` (`Idle`, `Assigned { vm_id, vcpu }`,
  `Revoking { donor…, recipient…, deadline_tsc }`) is owned by the
  engine thread alone.

### VMs that park inside KVM

A VMM started with `--pylon park=kvm` registers with
`wake_by_signal` set, and parks a vCPU inside KVM instead of on its wake
`eventfd`: the vCPU halts in `KVM_RUN`
([spec 08](https://github.com/CMKL-APEX/pylon/blob/main/spec/08-park-in-kvm.md)
§3). An interrupt then runs the guest again without a core and without
an exit; the vCPU stays `PARKED`. The engine treats such a VM
differently in four places: a due timer is no zero-core wakeup (phase 1), vCPUs
that KVM woke count as demand (phase 1), waking a vCPU for a core also
takes a signal (phase 2), and it tells `scx_pylon` which vCPUs are
parked (after phase 3).

The engine reads a vCPU's `state`, `yield_count`, `revoke_requested`,
`revoke_deadline_tsc`, `assigned_pcpu`, `park_on_boot`, `host_tid` and
`park_timer_deadline_tsc` from the VM's [host page](shared-memory-abi.md#the-host-page), which
the guest can't write, and writes `assigned_pcpu` and revocations there and to the
guest's copies. Below, those fields mean the host page's.

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
`tsc_khz` is the host counter rate that `pylond` finds at start
(`calibrate_tsc_khz`): `CNTFRQ_EL0` on aarch64, a 50 ms measurement
against the system clock on x86_64. `pylond` never reads the rate in a
VM's header, since the guest can write the shared page. If that fails, it
assumes the architectural rate or, on x86_64, 3,000,000.

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
is ahead of the guest's `rx_ring_cons_idx`. The two timers don't count
for a VM that parks inside KVM: KVM fires them, and the vCPU runs
without a core (spec 08 §3.2). For such a VM, a vCPU that KVM woke
(below) qualifies instead, once it is a long wake. A short wake runs on
the shared vCPU queue: a core for it would cost another VM a forced
preemption, and come back as soon as it halts.

**Woken vCPUs** (VMs that park inside KVM, spec 08 §4.2). The engine
reads the VM's `woken` mask from `scx_pylon`'s `pylon_vm_vcpus` map
(by the VM's slot; without `scx_pylon`, nothing writes it), keeps the vCPUs still
`PARKED` with no core assigned, and records when it first saw each one.
A vCPU woken for `WOKEN_DEMAND_US` (4 ms) or longer is a *long wake*.
The longest wake counts as waiting time, like `head_wait`. A VM that
owns a core and has no other unmet demand also wants one core per vCPU
woken now (`spare_only`), with urgency 0, so after every other VM, and
phase 3 gives it only spare cores: an idle one, or one held above its
floor by a VM whose desired cores are 0 (#52).

**Time bases.** Every `*_tsc` field counts host counter ticks (the TSC,
or `CNTVCT_EL0` on aarch64). The guest writes `head_wait_tsc` and
`next_timer_tsc` on its own counter. `pylond` converts them to the host
counter with
`PylonSharedPage::guest_to_host_tsc`, which subtracts the offset the
VMM measured for vCPU 0. Everything else is already host TSC.

**Desired cores:**

| Case | Desired cores |
|---|---|
| Zero-core wakeup | `max(runnable, 1)` clamped to `[max(min_cores, 1), max_cores]` |
| `runnable_threads > 0` | `runnable_threads` clamped to `[min_cores, max_cores]` |
| Otherwise | 0, whatever `min_cores`: it is a floor under demand, not a reservation |

Long wakes then raise desired cores to at least committed cores plus
the number of long wakes, up to `max_cores`. That catches demand `runnable_threads`
misses, such as work in the guest kernel or since the agent's last
report. Last, desired cores don't drop below the cores the VM has or is
getting for vCPUs that were woken when phase 3 picked them, until the
guest agent reports again (`guest_heartbeat` moves) or
`WOKEN_GRANT_HOLD_US` (50 ms) passes. Such a vCPU no longer counts as
woken once it has its core, and `runnable_threads` shows its work only
from the agent's next report: its VM would otherwise give the core back
in the next sweep, before the vCPU ran on it.

**Urgency** orders VMs in phase 3. `head_wait` is `now_tsc -
head_wait_tsc` (0 if the guest reports no waiting thread), or the
longest wake if that is longer:

| Case | Urgency |
|---|---|
| Zero-core wakeup | `u64::MAX` |
| Wants more cores than committed | `max(head_wait, 1) × priority_weight`, plus 2⁶⁰ if below `min_cores` |
| Otherwise | `head_wait × priority_weight` |

So a VM below its minimum outranks every VM above it, and among the
rest, priority weight scales how long a thread has waited.

## Phase 2: revocations in flight

For each `Revoking` CPU, the engine checks the donor's slot. Only the
VMM writes `PARKED`, once the donor's thread has left `KVM_RUN`, and it
increments `yield_count` first. The engine records the donor's
`yield_count` when it starts the revocation, and treats the donor as
parked only when it is `PARKED` with a higher count. A donor that was
just given its core is still `PARKED` from before, until its thread
wakes, and that doesn't count.

| Donor | Time | Action |
|---|---|---|
| `PARKED`, never signalled | any | A *cooperative yield*. Hand the CPU over. |
| `PARKED`, signalled | any | The VMM force-parked it. Hand the CPU over (already counted as forced). |
| not parked | before the deadline | Wait. |
| not parked | deadline passed | A *forced preemption*, counted once. Send the VMM's `kick_signal` to the donor's thread with `tgkill`, and again after `REVOKE_RESIGNAL_US` (10 µs), with the interval doubling each time up to `REVOKE_RESIGNAL_MAX_US` (1 ms). `KVM_RUN` returns `EINTR` and the VMM's post-exit hook force-parks the vCPU. |
| not parked | `FORCE_HANDOFF_US` (50 µs) after the first signal | Hand the CPU over anyway, counted as a *handoff without park*. `scx_pylon` preempts the donor thread, which parks at its next exit. |

The signal is resent because one that arrives while the vCPU thread is
outside `KVM_RUN` (handling an exit) doesn't stop the next guest entry.
The interval doubles because each signal also costs the guest an exit:
a guest the VMM can't park yet, with interrupts off, needs time in the
guest to turn them back on.
The 50 µs bound counts from the first signal, not the deadline, so a
donor always has that long after being signalled, even when the engine
runs late.

After a handoff without park, the donor still has `revoke_requested`
set and is no longer on any `Revoking` CPU. Each sweep therefore also
looks for such vCPUs and keeps signalling them until the VMM parks them,
at an interval that grows from the deadline on, up to 1 ms. These are vCPUs with `revoke_requested = 1`, no
`assigned_pcpu`, not `PARKED`, not tagged `park_on_boot` (Plane 1
signals boot parks), and a deadline that has passed. Without
this, a guest that never exits to userspace on its own kept running
without a core.
`pylond` signals only a VMM that registered a `kick_signal`, and checks
through a pidfd that the VMM process still exists before each
`tgkill`.

Handing the CPU over unassigns the donor (`assigned_pcpu`), writes the
recipient's thread ID (and its VM's slot, in the flags) to the CPU's
owner entry, sets the recipient slot's `assigned_pcpu`, writes its wake
`eventfd`, and marks the CPU `Assigned` to the recipient. For a VM that
parks inside KVM, the wake `eventfd` alone doesn't reach the thread
asleep in `KVM_RUN`, so the engine also sends `kick_signal`. Each sweep
sends it again, at most every `REVOKE_RESIGNAL_US` (10 µs), while the
vCPU is still `PARKED` with the `yield_count` it had at assignment: a
signal that lands just before the thread re-enters `KVM_RUN` interrupts
nothing. When the donor parked, the time from the
revocation request to `PARKED` goes into the VM's park latency
histogram. The engine doesn't touch the donor's `state` or
`revoke_requested`; the VMM clears `revoke_requested` when it parks.

If either VM has gone, the engine drops the revocation, marks the CPU
`Idle`, and releases the recipient's reservation.

## Phase 3: matching

The engine sorts the demand records by urgency, highest first (an
in-place insertion sort over at most 64 entries). Then, for each VM in
that order, while it needs more cores (desired minus committed):

1. **Pick a vCPU.** The lowest set bit of `stranded_work_bitmap` whose
   vCPU is free (clearing that bit): the guest tasks queued on it can't
   run until it has a core. Else the longest-woken free vCPU: it is
   already running, so it needs no wake-up. Else the lowest-numbered
   free vCPU whose parked timer is due, else the lowest-numbered free
   vCPU. A vCPU is *free* when it is
   `PARKED` and its `assigned_pcpu` is unassigned; a vCPU with a CPU on
   its way is still `PARKED` but already assigned or reserved. If the VM
   has no free vCPU, move on to the next VM.
2. **Fast path.** If a managed CPU is `Idle` and none of its SMT
   siblings is another VM's, assign it (as in phase 2's hand-over) and
   continue. A CPU whose sibling the VM already holds comes first.
3. **Preemption path.** Otherwise choose a donor with
   `select_donor_pcpu`, or stop matching for this VM if there's none.

### Donor selection

A CPU `Assigned` to another VM is an eligible donor if:

```text
none of the CPU's SMT siblings is assigned to, or being handed to or
    from, a VM other than the recipient
and donor's vCPU isn't PARKED
and ((donor above its min_cores
        and (donor holds more than it desires
             or donor holds more than recipient's committed cores + 1
             or (donor's urgency < recipient's urgency
                 and (recipient's committed + 1) × donor's weight
                     ≤ donor's cores × recipient's weight)))
     or (recipient is a zero-core wakeup
         and recipient took no core from a donor at its min_cores
             in the last FLOOR_PREEMPT_INTERVAL_US
         and donor's urgency < recipient's urgency))
```

A zero-core wakeup has the highest urgency, and the timers and
telemetry behind it come from the guest. So a VM woken from zero cores
takes a core from a donor at its `min_cores` at most once every
`FLOOR_PREEMPT_INTERVAL_US` (10 ms). A guest that parks and wakes again
and again otherwise keeps taking cores below other VMs' floor.

A `PARKED` vCPU on an assigned CPU either hasn't woken since it was
given the CPU, or has given it back and phase 0 will reclaim it.
Revoking it would race its wakeup.

Urgency alone moves a core only while the donor keeps at least as
many cores per unit of `priority_weight` as the recipient will have.
Without that bound, two hungry VMs with equal weights traded a core on
every sweep: the more urgent one took the other below an even split,
then *more than committed + 1* gave it back. With the bound, an even
split is final, and a VM with twice the weight ends up with two cores
for every one.

Among eligible donors, the engine prefers, in order:

1. a vCPU that isn't in a critical section;
2. a VM holding more than it desires;
3. the lowest urgency.

It then sets the donor slot's `revoke_deadline_tsc` to now plus
`REVOKE_GRACE_PERIOD_US` (8 µs, converted with the host's `tsc_khz`),
stores `revoke_requested = 1`, writes the donor's kick `eventfd`, and
marks the CPU `Revoking`. It reserves the recipient vCPU by setting its
`assigned_pcpu` to that CPU, so no other match picks it. The recipient's
incoming count goes up, and the donor's assigned count goes down, so
later decisions in the same sweep see the change.

## After phase 3: parked vCPUs for `scx_pylon`

Last, for each VM that parks inside KVM, the engine writes the mask of
its vCPUs that are `PARKED` with no CPU assigned to the `parked` field
of its `pylon_vm_vcpus` entry, and clears the entries of VMs that have
gone. This runs after matching, so the vCPUs given a CPU in this sweep
are already out of the mask. `scx_pylon` uses it to tell a parked vCPU
that KVM woke from one that runs on its own CPU
([The scx_pylon dispatcher](scx-pylon.md#callbacks)).

## The owner table

`PcpuOwnerTable` is an array of `{ target_tid: u32, flags: u32 }`
indexed by host CPU. Bits 16–31 of `flags` hold the owner's VM slot
plus one (0 if the VM has no slot); `scx_pylon` finds the CPUs a VM
owns by it. It lives in one of three places:

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
| `wake_latency`, `handoff_latency` (histograms, µs) | `VmHandle` | Phase 2, the first sweep an assigned vCPU is seen running: since the assignment, and since the revocation request if there was one |
| `woken_shared_usec`, `woken_unowned_usec` | `VmHandle` | Phase 1: time vCPUs spent woken, while the VM owned a core or none |

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
