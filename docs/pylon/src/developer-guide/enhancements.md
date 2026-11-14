# Enhancements

This page covers the changes to Pylon's revocation path, what is done
and what is left: first the work to make revocation cooperative, then
parking inside KVM, then smaller and later items. The authorities are
[`spec/06-guest-kick-path.md`](https://github.com/CMKL-APEX/pylon/blob/main/spec/06-guest-kick-path.md)
and
[`spec/08-park-in-kvm.md`](https://github.com/CMKL-APEX/pylon/blob/main/spec/08-park-in-kvm.md)
(both drafts). Progress is tracked in
[`spec/05-implementation-status.md`](https://github.com/CMKL-APEX/pylon/blob/main/spec/05-implementation-status.md)
as tracks 4, 5 and 7. Open work is tracked as GitHub issues on the
[`v0.0.8` and `v0.0.9` milestones](https://github.com/CMKL-APEX/pylon/milestones).

## Problems in the original revocation path

The handoff in [Architecture](architecture.md#flow-handing-a-core-to-a-vm)
expects the donor vCPU to be interrupted, finish any critical section,
and park. Six problems prevented that. Spec 06 steps S1–S4 fixed all
six (spec 05, "Known Issues"):

| # | Problem | Effect |
|---|---|---|
| P1 | The VMM creates per-vCPU kick `eventfd`s and sends them to `pylond`, which writes them, but nothing turns them into a guest interrupt. | The guest is never told about a revocation. |
| P2 | The park hook for HLT sits on `VmExit::Reset`. KVM handles HLT in the kernel, and also reports a guest triple fault as `Reset`. | The hook never fires as intended, and a triple fault during a revocation is treated as a park instead of a reset. |
| P3 | The VMM parks on any exit to userspace while `revoke_requested` is set, without checking `in_critical_section`. | A vCPU can be parked while it holds a guest lock. |
| P4 | `pylon-guest-agent` marks other vCPUs `PARKED` and clears their `revoke_requested` while they keep running. | `pylond` hands the CPU over while the donor still runs, and the VMM never parks it. |
| P5 | When the deadline passes, `pylond` writes `PARKED` and clears `revoke_requested` for a vCPU that is still runnable. | The donor never parks. It only moves elsewhere under the host scheduler. |
| P6 | `state` has three writers and no ownership rules. | P4 and P5. |

## The design

### Field ownership

Every field gets one writer (spec 06 §3), and `PYLON_ABI_VERSION` went
to 2 because the writer rules changed, although the layout didn't:

| Field | Writer |
|---|---|
| `state` | The guest, only `RUNNING → PARKING`, for its own vCPU. The VMM, everything else. **`pylond` never writes it.** |
| `revoke_requested` | `pylond` sets it; the VMM clears it when it parks the vCPU. |
| `forced_preempt_count` | Moves to the VMM, the only party that knows a forced park happened. |
| `yield_count`, `last_yield_tsc` | The VMM. |

### Kick delivery: the `pylon-kick` PCI device

A small virtual PCI device, created with `--pylon`, with one MSI-X
vector per vCPU (spec 06 §4.2). The guest driver pins vector *i* to
CPU *i*, so vector *i* interrupts vCPU *i*. The device
installs the VMM's kick `eventfd`s, the ones `pylond` already has, as
its MSI-X notifiers, so a write from `pylond` goes through the irqfd
path and arrives as an interrupt. BAR0 exposes the magic, ABI version,
vCPU count and the shared page's address, plus a park doorbell; BAR1
holds the MSI-X table.

**Implemented (S4)** in `vmm/src/pylon_kick.rs`, as `1af4:10f0` (Q1).
While the guest hasn't enabled a vector, or has masked it, a kick waits
in its `eventfd` and is delivered once the vector is enabled. A VM with
`--pylon` now refuses to snapshot or migrate. The guest driver is
`pv-pylon` (S5).

Alternatives the spec rejected: a raw vector chosen by the guest (Linux
has no supported way for a module to own one), `KVM_SIGNAL_MSI` from
`pylond` (it doesn't hold the VM's descriptor), ivshmem-doorbell (Cloud
Hypervisor implements only ivshmem-plain), and a virtio device (too
heavy for one doorbell and N interrupts).

### Cooperative park: a doorbell, then a decision table

At a safe point, the guest writes `state = PARKING` and then any value
to the `PARK_DOORBELL` register. That MMIO write exits to the VMM on
the writing vCPU, so the vCPU runs no more guest code. The device's
handler does nothing (it must not block while holding the device's
lock). The existing post-exit hook decides (spec 06 §4.4):

| `state` | `revoke_requested` | Deadline | Action |
|---|---|---|---|
| `PARKING` | any | any | Cooperative park. A park without a revocation (an idle guest giving a core back) is allowed. |
| `RUNNING` | 1 | passed | Forced park; `forced_preempt_count += 1`. |
| `RUNNING` | 1 | not passed | Return: the guest hasn't agreed and still has time. |
| `RUNNING` | 0 | — | Return (the common case: one atomic load). |

The `VmExit::Reset` hook was removed (S1).

### Forced park: a signal from pylond

**Implemented (S2).** When a deadline passes, `pylond` sends
`tgkill(vmm_pid, host_tid, kick_signal)` (spec 06 §4.4–4.5). Cloud
Hypervisor's vCPU threads already ignore `SIGRTMIN` with a no-op
handler, and the VMM gives them the same handler for `kick_signal`, so `KVM_RUN` returns `EINTR`, the KVM backend reports
`VmExit::Ignore`, and the post-exit hook force-parks. Details:

- The VMM sends `SIGUSR2` as `kick_signal` when it registers. Unlike a
  real-time signal, it doesn't queue, so signals sent while a vCPU waits
  to run cost one handler. `pylond` accepts `SIGUSR1`, `SIGUSR2` or a
  real-time signal, and doesn't signal a VMM that sent none.
- `pylond` learns the VMM's PID from `SO_PEERCRED` and opens a pidfd for
  it. Before each signal it checks the process still exists, so a reused
  PID is never signalled.
- The signal is resent, because one that arrives while the thread is
  outside `KVM_RUN` doesn't stop the next guest entry. The interval
  starts at 10 µs and doubles with each signal, up to 1 ms. A thread
  that has left `KVM_RUN` since the last signal isn't signalled again
  until it re-enters (`kvm_run_seq`).
- `pylond` hands the CPU over only once the VMM has written `PARKED`. As a
  last resort it hands it over 50 µs after the deadline, counted
  separately as a handoff without park.
- `pylond` no longer writes `WAKING`. It reserves a recipient through
  `assigned_pcpu` and reclaims a core only after the vCPU's `yield_count`
  has moved ([The arbitration engine](arbitration.md#phase-2-revocations-in-flight)).

### The guest kernel module

`pv-pylon` (`pylon/guest/linux/`, spec 06 §4.6) binds to the device,
maps the page, and allocates one MSI-X vector per vCPU.

- **Vectors.** Vector *i* is pinned to CPU *i* whenever that CPU comes
  online. The kernel's managed spreading isn't used, because it doesn't
  keep that order.
- **The kick.** The kick handler wakes CPU *i*'s `SCHED_FIFO` park
  thread. A kick that lands on another CPU is forwarded to that thread,
  not dropped.
- **The park.** The thread runs at a safe point. It sets the vCPU's
  `stranded_work_bitmap` bit if other tasks are waiting, writes
  `state = PARKING`, then writes the doorbell. The vCPU stops at that
  write until `pylond` wakes it.
- **`/dev/pylon`.** The module exposes the page there, and
  `pylon-guest-agent` maps it instead of `/dev/mem`. The agent has been
  telemetry-only since S3.

Still to do: keeping `in_critical_section` up to date, and making other
vCPUs treat parked ones as preempted. Neither can be done from a module;
both need guest kernel changes (spec 06 Q3,
[#56](https://github.com/CMKL-APEX/pylon/issues/56)).

## Implementation steps

| Step | Scope | Fixes |
|---|---|---|
| S1 | VMM: the decision table; remove the `Reset` hook. **Done.** | P2, P3 |
| S2 | `pylond`: stop writing `state`; `SO_PEERCRED`; `tgkill` on deadline; wait for `PARKED`; latency metric. **Done.** | P5, P6 (`pylond`'s part) |
| S3 | Guest agent telemetry-only; ABI version 2. **Done.** | P4, P6 |
| S4 | The `pylon-kick` device; the VMM's kick `eventfd`s become its MSI-X notifiers. **Done.** | P1 |
| S5 | The `pv-pylon` kernel module. **Done** (spec 06 §4.6 items 1–4), except critical sections and `vcpu_is_preempted` ([#56](https://github.com/CMKL-APEX/pylon/issues/56)). | — |
| S6 | End-to-end latency: kick to `PARKED`, and revocation to recipient running. **Not started** ([#53](https://github.com/CMKL-APEX/pylon/issues/53)). | — |

S1–S5 have landed. With `pv-pylon` loaded, a revoked vCPU parks itself.

- **Stand-in `pylond`.** All 800 test revocations parked cooperatively
  on Ubuntu 6.8 and 7.0 guests.
- **Not inside the grace period yet.** Kick to `PARKED` takes about
  20 µs, measured coarsely. That's longer than the 8 µs grace, so under
  the real `pylond` many revocations still end in a forced park. S6 is
  about that latency.
- **The real-`pylond` run** first hit a `pylond` bug with boot parks
  (spec 05 K7, now fixed). It then settled cores, and the guests
  stalled on a vCPU that stayed parked (Q2). Parking inside KVM
  answers Q2 for unmodified guests (next section). The real-`pylond`
  check of cooperative parks is still to do
  ([#55](https://github.com/CMKL-APEX/pylon/issues/55)).
- **S4 uses a provisional PCI ID** (Q1,
  [#64](https://github.com/CMKL-APEX/pylon/issues/64)).

## Parking inside KVM (spec 08)

A vCPU parked on its wake `eventfd` blocks in the VMM, outside
`KVM_RUN`. Interrupts sent to it wait until `pylond` gives it a core,
and the guest can't tell it isn't running. In a two-VM run on
2026-10-31 this led to soft lockups and RCU stalls within 30 s
(spec 08 §1). Spec 08 parks the vCPU inside KVM instead, so KVM wakes
it like a vCPU the guest halted itself. It needs no guest changes.

The VMM uses it with `--pylon park=kvm`, the default on x86_64 KVM
hosts, and only on KVM. It relies on `scx_pylon`: `pylond` without it
rejects such a VM (spec 08 §2). How it works:

- **Park.** The VMM takes the usual steps up to `PARKED`, then sets the
  vCPU halted (`KVM_MP_STATE_HALTED` on x86_64, `KVM_MP_STATE_SUSPENDED`
  on aarch64) and calls `KVM_RUN` again. KVM sleeps the thread. Halt
  polling is off for the VM (`Vm::set_halt_poll_ns(0)`).
- **Wake by interrupt.** An IPI, the guest's timer or a device MSI wakes
  the vCPU inside KVM. The VMM and `pylond` don't see this. The vCPU
  stays `PARKED` and runs without a core of its own.
- **Wake for a core.** `pylond` signals the thread, as for a forced
  park. `KVM_RUN` returns `EINTR`, and the VMM sees `assigned_pcpu`,
  sets the vCPU runnable and writes `RUNNING`.
- **Interrupts off (x86_64).** At a forced park the VMM checks
  `RFLAGS.IF`. If interrupts are off it doesn't park, and `pylond`
  signals again later.
- **Bounded park.** A parked vCPU that nothing wakes runs again after
  4 ms (`PARK_IN_KVM_BOUND`), still without a core. A boot park has no
  bound.
- **`scx_pylon`.** The engine publishes which vCPUs are parked in the
  `pylon_vm_vcpus` map, and `scx_pylon` marks those it sees runnable as
  woken. A woken vCPU shares its own VM's CPUs in short turns. If its VM
  owns none, it runs on the shared vCPU queue.
- **`pylond`.** It reads the woken bits each sweep. A vCPU woken for
  longer than 4 ms (`WOKEN_DEMAND_US`) counts as demand for a core,
  and a VM with no core gets a zero-core wakeup then. A shorter wake, in
  a VM that owns a core, takes only a spare core: an idle one, or one
  held for at least 1 s by a VM that wants none. A core given to a
  woken vCPU stays wanted until the guest agent reports again, or for
  50 ms (`WOKEN_GRANT_HOLD_US`). Woken time is metered per VM
  (`woken_shared_usec`, `woken_unowned_usec`).

| Step | Scope | Status |
|---|---|---|
| P0 | VMM only: park inside KVM, wake by signal, halt polling off. | **Done.** No soft lockups in the two-VM run (spec 08 §7.1). |
| P1 | `scx_pylon`: per-VM queues and the woken map. | **Done.** Woken vCPUs run only on their VM's CPUs (§7.2). |
| P2 | `pylond`: read the woken map, meter shared time, treat long wakes as demand. | **Done.** No stalls in the two-VM runs (§7.3). |
| P3 | Measurements: wake-to-run latency for IPIs to parked vCPUs, guest throughput against the `eventfd` park, halt-polling settings. | **Done.** No stalls, throughput at the baseline's, halt polling stays off (§7.4). |

Still open:

- **Most parks end by the 4 ms bound**, not by a wake
  ([#52](https://github.com/CMKL-APEX/pylon/issues/52)).
- **Snapshot and migration** stay refused.

## Open questions

These are from spec 06 §10:

- **Q1.** The PCI vendor and device ID for `pylon-kick`. For now it is
  `0x1AF4:0x10F0`, from the range that QEMU's PCI ID list leaves free for
  experiments. That range isn't for shipped products, so a PCI-SIG vendor ID
  for CMKL is still to be requested, with a device ID under it before
  release ([#64](https://github.com/CMKL-APEX/pylon/issues/64)).
- **Q2.** Interrupts sent to a parked vCPU. A guest waiting for a TLB
  shootdown to finish spins until `pylond` wakes the target. Options:
  mark parked vCPUs preempted so the guest defers the flush, have the
  guest flag the target in `stranded_work_bitmap`, or have the VMM
  watch for pending interrupts on parked vCPUs. This also affects vCPUs
  parked at boot. **Answered for unmodified guests** by parking inside
  KVM ([above](#parking-inside-kvm-spec-08)), the default on x86_64
  KVM hosts.
- **Q3.** The minimum guest kernel version, and where to hook
  `in_critical_section`. `pv-pylon` builds against Linux 6.3 and later;
  the hook points are open
  ([#56](https://github.com/CMKL-APEX/pylon/issues/56)).
- **Q4.** Whether MSHV reports a signal-interrupted run the way KVM does
  ([#65](https://github.com/CMKL-APEX/pylon/issues/65)).
- **Q5.** *Answered:* how a VMM reconnects after `pylond` restarts.
  It keeps its registration open as a session, runs the VM unmanaged
  while `pylond` is away, and registers again when it is back
  ([spec 10](https://github.com/CMKL-APEX/pylon/blob/main/spec/10-pylond-restart.md),
  [#60](https://github.com/CMKL-APEX/pylon/issues/60)).
- **Q6.** *Answered:* the last-resort bound is a host-wide `pylond`
  option, `--force-handoff-us` (default 50 µs), not per-VM policy: one
  VM's value would delay another's handoff
  ([#60](https://github.com/CMKL-APEX/pylon/issues/60)).

## Other planned work

| Item | Notes |
|---|---|
| Move the shared page into the `pylon-kick` BAR | Removes the fixed address (spec 06 §9, [#59](https://github.com/CMKL-APEX/pylon/issues/59)). |
| Receive doorbell | A host-side producer for `host_rx_doorbell`, so packets wake a zero-core VM (K1, [#57](https://github.com/CMKL-APEX/pylon/issues/57)). |
| Lifecycle | Snapshot, restore and migration with `--pylon`. They are refused today, through the `pylon-kick` device (spec 06 §7); supporting them is [#61](https://github.com/CMKL-APEX/pylon/issues/61). |
| aarch64 | Done for the host side (spec 07, A-T0 to A-T7): a counter abstraction, `WFET`, the guest timer, the shared page's address in the aarch64 layout, an arm64 `scx_pylon` and CI. Open: a booted aarch64 guest run end to end (A-T8, in progress, [#62](https://github.com/CMKL-APEX/pylon/issues/62)) and the kick device on the GICv3 ITS (A-T9, not started, [#63](https://github.com/CMKL-APEX/pylon/issues/63)). |
| `scx_pylon` lookup | A thread-ID-to-CPU map, so `select_cpu` doesn't scan all 256 entries. |

Done since the first draft of this list: the real TSC rate (`pylond`
measures the host's; K2), removing exited VMs and freeing their CPUs
(K6), rejecting duplicate `vm_id`s, and `--socket-group`/`--socket-mode`
(K5); one time base, with the VMM measuring each vCPU's guest TSC offset
(K3); keeping the page out of the guest's RAM map (K4); and waking a
parked vCPU when its guest timer is due (part of K1).
