# Specification 06: Guest Kick & Cooperative Park Path

> **Status:** `DRAFT` — design for review. Steps S1 (§4.4, the VMM's park decision),
> S2 (§4.4–§4.5, `pylond`'s forced park) and S3 (§3, §4.6: telemetry-only guest agent,
> ABI version 2) are implemented; the rest isn't. Where the implementation refined the
> design, the affected sections say so.
> Tracked as **Track 4** (VMM/pylond) and **Track 5** (guest kernel) in
> `spec/05-implementation-status.md`.

## 1. Motivation: Gaps in the Current Revocation Path

`spec/00-architecture.md §3` describes a donor vCPU that gets a revoke interrupt,
finishes any critical section, parks, and hands its pCPU to a recipient within
the 8 µs grace window. As of `ea96e7195`, that loop doesn't close. The
problems:

| # | Problem | Location | Effect |
| :- | :--- | :--- | :--- |
| P1 | Per-vCPU kick `EventFd`s are created and sent to `pylond`, but nothing on the VMM side ever reads them or registers them as KVM irqfds. | `vmm/src/pylon.rs` (`kick_eventfds`, `kick_eventfd()` has no callers); `pylond/src/engine.rs` (`kick_revoke_irqfd`) | Revoke requests never reach the guest as interrupts. The guest only notices `revoke_requested` if something happens to poll it. |
| P2 | The "HLT fast-park" hook depends on HLT exits reaching userspace. KVM handles HLT in-kernel with an in-kernel LAPIC. The hook is attached to `VmExit::Reset`, which the KVM backend also returns for triple faults (`VcpuExit::Shutdown`). | `vmm/src/cpu.rs` (`VmExit::Reset` arm); `hypervisor/src/kvm/mod.rs` (`VcpuExit::Hlt`/`Shutdown` → `Reset`) | The fast-park path never runs in practice. Worse, a guest triple fault that arrives while `revoke_requested == 1` gets swallowed as a park instead of resetting the VM. |
| P3 | The post-exit park check parks on *any* userspace exit while `revoke_requested == 1`, without looking at `in_critical_section`. | `vmm/src/pylon.rs` `check_and_park_vcpu` | A vCPU that holds a guest spinlock and does an MMIO/PIO access gets parked mid-critical-section. That is the lock-holder preemption problem the design is meant to avoid. |
| P4 | The userspace guest agent writes `state = PARKED` and clears `revoke_requested` for *other* vCPUs while those vCPUs are still executing. | `pylon-guest-agent/src/lib.rs` `handle_revocations` | `pylond` treats the pCPU as free while the donor thread is still in `KVM_RUN`. The VMM never parks the donor, because the flag it checks has already been cleared. |
| P5 | On deadline expiry, `pylond` writes `state = PARKED` and clears `revoke_requested` for a vCPU that is still runnable. | `pylond/src/engine.rs` Phase 2 (`expired && !parked`) | The donor never parks. It is only migrated by the host scheduler, and its slot claims to be parked. |
| P6 | `PylonVcpuSlot::state` is written by the guest agent, the VMM vCPU thread, and `pylond`. There are no ownership rules. | all of the above | The races and lies in P4/P5 follow from this. |

This spec closes P1–P6. It defines field ownership, a kick-delivery device, a
synchronous park doorbell, and a host-enforced deadline fallback.

---

## 2. Goals & Non-Goals

**Goals**
1. Deliver a revoke kick to a running donor vCPU as a guest interrupt, routed
   to that vCPU, within ~2 µs of `pylond` setting `revoke_requested`.
2. Park the donor only at a point the guest declares safe (cooperative path),
   or after `revoke_deadline_tsc` expires (forced path).
3. `pylond` hands a pCPU to the recipient only after the donor thread has
   actually left `KVM_RUN`, as reported by the VMM.
4. Use standard Linux guest mechanisms (PCI MSI-X with managed affinity)
   instead of custom vector allocation, so the guest side can be a normal
   driver.
5. Keep the device and park logic independent of architecture and hypervisor
   backend, even though the first target is x86_64 + KVM.

**Non-Goals (this spec)**
- Moving the shared page from the fixed GPA `0xFEDF_0000` into a PCI BAR. This
  is a natural follow-up (see §9), but it is not needed to close P1–P6.
- Snapshot/restore and live migration with `--pylon` (rejected explicitly in
  §7).
- aarch64 support for `pylond` (it uses `_rdtsc`/`_tpause` today).

---

## 3. Field Ownership Rules (ABI v2)

The memory layout is unchanged. The **writer rules** change, so
`PYLON_ABI_VERSION` is bumped to `2`. The VMM writes the version, and a guest
component that doesn't recognise it must stay telemetry-only.

| Field | Writer(s) | Rule |
| :--- | :--- | :--- |
| `state` | **Guest** on the vCPU itself: `RUNNING → PARKING` only. **VMM** vCPU thread: everything else. | The guest writes `PARKING` only for *its own* vCPU, immediately before the park doorbell (§4.3). The VMM writes `PARKED` once the thread has left `KVM_RUN` and is about to block, and `RUNNING` after it wakes. **`pylond` never writes `state`.** |
| `in_critical_section` | Guest (own vCPU only) | Same as today. |
| `revoke_requested` | `pylond` sets `1`. **VMM** clears it to `0` when it parks the vCPU. | The guest never clears it. `pylond` clears it only if it cancels a revocation before the donor parked (e.g. the recipient's demand went away). |
| `revoke_deadline_tsc` | `pylond` | Written *before* `revoke_requested = 1` (Release). |
| `assigned_pcpu` | `pylond` | Unchanged. |
| `forced_preempt_count` | VMM (increments it when it force-parks, §4.4) | Moves from `pylond` to the VMM, because the VMM is the only party that knows a forced park actually happened. |
| `host_tid` | VMM | Unchanged. |
| `last_yield_tsc`, `yield_count` | VMM | Written when the VMM parks the vCPU (cooperative or forced). |
| `park_timer_deadline_tsc` | VMM | The guest's armed TSC-deadline timer, in host TSC, written before `PARKED` and cleared on wake (K1). Uses the former `_reserved_guest`. |
| `guest_tsc_offset` | VMM | Guest TSC minus host TSC, measured before the vCPU first runs (K3). Uses the former `_reserved_host`. |

`VCPU_STATE_WAKING` is no longer written by `pylond`. The VMM may write it
between returning from the wake `EventFd` read and re-entering `KVM_RUN`, if
that window turns out to be useful for metering. Otherwise it is unused.

---

## 4. Design

### 4.1 End-to-End Sequence

```text
pylond (Core 1)                 VMM donor vCPU thread            Guest (donor vCPU i, pv-pylon)
---------------                 ---------------------            ------------------------------
deadline_tsc = now + 8us
revoke_requested = 1 (Release)
write(kick_irqfd[i]) ──────────────────── KVM MSI-X route ──────► IRQ on vCPU i
                                                                  if in_critical_section:
                                                                     defer to section exit
                                                                  at safe point:
                                                                     state = PARKING
                                                                     MMIO write PARK_DOORBELL
                                 KVM_EXIT_MMIO ◄───────────────────
                                 device handler: no-op ack
                                 drop(vcpu lock)
                                 post-exit hook sees PARKING:
                                   revoke_requested = 0
                                   state = PARKED (Release)
                                   block on wake_eventfd[i]
state == PARKED observed  ◄──────
pcpu_owner[x] = recipient_tid
wake recipient
```

**Forced path:** if `now >= deadline_tsc` and `state != PARKED`, `pylond` sends
`tgkill(vmm_pid, host_tid, SIGRTMIN)` (§4.4). `KVM_RUN` returns `EINTR`, which
the KVM backend maps to `VmExit::Ignore` (`hypervisor/src/kvm/mod.rs`). The
post-exit hook then sees the expired deadline and force-parks.

### 4.2 Kick Delivery: `pylon-kick` Virtual PCI Device

A small PCI device, created when `--pylon` is set:

- **MSI-X:** `num_vcpus` vectors, one per vCPU. The guest driver allocates them
  with managed affinity (`pci_alloc_irq_vectors_affinity` / `PCI_IRQ_AFFINITY`),
  so vector *i* targets vCPU *i*. The guest programs the MSI address/data, so
  the VMM needs no knowledge of APIC IDs or vectors.
- **Kick FDs:** the VMM keeps its own per-vCPU `kick_eventfds`, which it sends
  to `pylond` at registration. The device creates its MSI-X group with
  `create_group_mut` and installs them as the vectors' notifiers
  (`InterruptSourceGroup::set_notifier` in `vm-device/src/interrupt/mod.rs`).
  When `pylond` writes one, the irqfd route delivers the MSI. No new hypervisor
  plumbing is needed, and the same path works with MSHV's interrupt manager.

  *As implemented (S4):* the first design took the group's own notifiers and
  sent those instead. That would have made registration wait for the device,
  and a guest reboot, which creates a new device, would have left `pylond`
  with dead `EventFd`s. With the VMM's `EventFd`s, registration stays where it
  is and survives a reboot.
- **Masked vectors:** Cloud Hypervisor's `MsixConfig` only reroutes unmasked
  vectors and leaves it to a device to check the mask before it triggers one.
  `pylond` writes the `EventFd` directly, so the device removes a vector's irqfd
  when the guest masks it. A kick then waits in the `EventFd`, and KVM delivers
  it when the guest unmasks the vector, as PCI's pending bit would. Disabling
  MSI-X, or setting the function mask, removes all of them.
- **BAR0 (MMIO, 4 KiB):**

  | Offset | Name | Access | Semantics |
  | :--- | :--- | :--- | :--- |
  | `0x00` | `MAGIC` | RO | `PYLON_MAGIC` low 32 bits |
  | `0x04` | `ABI_VERSION` | RO | `PYLON_ABI_VERSION` (2) |
  | `0x08` | `NUM_VCPUS` | RO | `header.num_vcpus` |
  | `0x0C` | `SHM_GPA_LO` | RO | Low 32 bits of the shared page GPA |
  | `0x10` | `SHM_GPA_HI` | RO | High 32 bits of the shared page GPA |
  | `0x40` | `PARK_DOORBELL` | WO | Any write causes a synchronous exit (§4.3). The written value is the vCPU index and is used only for tracing. |

  The GPA registers let the guest driver find the shared page without hardcoding
  `0xFEDF_0000`. This makes the later BAR move (§9) a device-side change only.
- **BAR1 (MMIO, 4 KiB):** the MSI-X table in the first half and the pending bit
  array in the second, so the registers in BAR0 don't share a page with them.
- **Snapshots:** the device refuses to snapshot, so a VM with `--pylon` can't
  be snapshotted or migrated (§7).

**Why a PCI device instead of the alternatives:**
- *Guest-allocated raw vector + VMM-programmed MSI:* Linux has no supported way
  for a module to own a fixed IDT vector, and the VMM would need guest APIC ID
  mappings.
- *`KVM_SIGNAL_MSI` from `pylond`:* `pylond` doesn't hold the VM fd, and the
  ioctl costs a syscall on the hot path. An irqfd write is also a syscall, but
  it needs no VM fd and is already what the engine does.
- *ivshmem-doorbell:* that is QEMU's equivalent, but Cloud Hypervisor's
  `devices/src/ivshmem.rs` implements only ivshmem-plain.
- *A virtio device:* too heavy for a single doorbell and N interrupts.

**PCI IDs:** `0x1AF4:0x10F0` for now. QEMU's `docs/specs/pci-ids.rst`
leaves `1af4:10f0` to `1af4:10ff` in Red Hat's vendor ID "available for
experimental usage without registration", and requires an official ID "when
the code leaves the test lab (i.e. when seeking upstream merge or shipping a
distro/product)". Shipping Pylon in Glidex Cloud counts, so before release
`pylon-kick` moves to a device ID under a CMKL vendor ID (Q1). Rules until then:

- Keep the vendor and device ID in one constant, so the move is a one-line
  change in the VMM and in `pv-pylon`'s ID table.
- The range is shared with anyone else's experiments, so the guest driver
  checks `MAGIC` and `ABI_VERSION` in BAR0 before it binds. The subsystem IDs
  repeat the vendor and device ID: without an allocation, no subsystem ID would
  be Pylon's own either.
- We must not take any other ID under another vendor's ID without an
  allocation.

A guest kernel enumerates the device at boot whatever its ID. No upstream
driver binds `1af4:10f0`, and `pv-pylon` is autoloaded through its
`MODULE_DEVICE_TABLE` modalias.

### 4.3 Park Doorbell (Cooperative Path)

The park must be **synchronous on the parking vCPU**. Once the guest decides to
park, that vCPU must not run any more guest instructions until it is woken.
An MMIO write to an unmapped BAR page gives exactly that: `KVM_EXIT_MMIO` is
taken on the writing vCPU, and the instruction completes only on the next
`KVM_RUN`. MMIO is used instead of PIO so the same mechanism works on aarch64.

Rules:
1. The BAR0 write handler does **not** block. It runs under the device's
   `Mutex` on the bus, so blocking there would serialize every vCPU that touches
   the device. The handler does nothing except return; the exit itself is the
   signal.
2. Parking happens in the existing post-exit hook in `vmm/src/cpu.rs` (after
   `drop(vcpu)`), which already runs on every userspace exit.
3. The guest must write `state = PARKING` (Release) **before** the doorbell
   write, and only for the vCPU it is running on.

### 4.4 Post-Exit Park Decision (VMM)

`check_and_park_vcpu(vcpu_id)` becomes this decision table, evaluated after
every userspace exit:

| `state` | `revoke_requested` | Deadline | Action |
| :--- | :--- | :--- | :--- |
| `PARKING` | any | any | **Cooperative park.** Clear `revoke_requested`, bump `yield_count`/`last_yield_tsc`, write `PARKED`, block on wake. A voluntary park (no revoke) is allowed, e.g. when the guest is idle. |
| `RUNNING` | `1` | `now >= revoke_deadline_tsc` | **Forced park.** Same steps, plus `forced_preempt_count += 1`. |
| `RUNNING` | `1` | not expired | Return immediately. The vCPU didn't consent and still has time. (This fixes P3.) |
| `RUNNING` | `0` | — | Return immediately (fast path: one Acquire load). |

On wake, the VMM writes `RUNNING` (unless shutting down), the same as today.

The `VmExit::Reset` hook added in `dff66af86` is **removed**. `Reset` goes back
to always meaning reset (fixes P2).

The forced exit comes from `pylond`, as follows. The control plane records the
VMM's PID from `SO_PEERCRED` on the registration socket (tokio
`UnixStream::peer_cred()`). On deadline expiry, Plane 2 calls
`tgkill(vmm_pid, host_tid, kick_signal)`. Cloud Hypervisor already installs a
no-op `SIGRTMIN` handler on vCPU threads (`vmm/src/cpu.rs` `handle_signal`), so
this interrupts `KVM_RUN` without other side effects. Requirements: `pylond`
runs as the same UID as the VMM or holds `CAP_KILL`. Using `tgkill` instead of
`tkill` only signals `host_tid` if it is a thread of `vmm_pid`.

*As implemented (S2):*

- **The VMM names the signal.** `SIGRTMIN` depends on the libc (glibc and musl
  reserve different numbers of real-time signals), and a real-time signal
  without a handler terminates the process. So the VMM sends its own
  `SIGRTMIN()` as `kick_signal` in the registration message. `pylond`
  rejects a registration whose `kick_signal` isn't a real-time signal, and
  never signals a VMM that didn't send one (an older VMM).
- **PID reuse.** At registration, `pylond` opens a pidfd for the peer PID
  (`VmHandle::with_vmm_pid`). Before each `tgkill`, it checks with
  `pidfd_send_signal(pidfd, 0)` that the process still exists, so a PID
  reused after the VMM exits is never signalled.
- **The VMM's post-exit hook** treats `now >= revoke_deadline_tsc` with
  `revoke_requested == 1` as a forced park, reading the host TSC. `pylond`
  sends the signal no earlier than the deadline, so the VMM always sees it as
  passed.

### 4.5 `pylond` Phase 2 Changes

- Stop writing `state`, `forced_preempt_count`, and `revoke_requested = 0` on
  expiry (fixes P5).
- When the deadline expires, send the `tgkill` above **once**, and move the core
  to a new `CoreState::ForceRevoking { .., signalled_tsc }` substate.
- Hand the pCPU to the recipient only when the donor's `state == PARKED`. Keep
  a second hard bound, `FORCE_HANDOFF_US` (proposed 50 µs after the signal).
  Past that bound, hand over anyway and rely on `scx_pylon` preemption, and
  count it as `handoff_without_park`, a separate metric that should stay at
  zero in healthy operation.
- Metering: record kick→`PARKED` latency per VM (p50/p99) using the Prometheus
  exporter from `7bd0b5815`.

*As implemented (S2), with these refinements:*

- **Per-core arrays instead of a new `CoreState` variant.** `MatrixEngine`
  keeps `revoke_requested_tsc` and `revoke_signalled_tsc` per managed CPU, so
  `CoreState` and its users are unchanged.
- **The signal is resent** every `REVOKE_RESIGNAL_US` (10 µs) until the donor
  parks. A signal that arrives while the vCPU thread is outside `KVM_RUN`
  (handling an exit) is consumed by the no-op handler and doesn't stop the
  next guest entry, so a single signal can be lost.
- **`FORCE_HANDOFF_US` (50 µs) counts from the deadline**, which is when the
  first signal is sent.
- **Counting.** A revocation is counted as forced once, when its deadline
  passes, and as cooperative when the donor parked before that. The park
  latency (request to `PARKED`) goes into a per-VM histogram,
  `pylon_vm_park_latency_microseconds`, with buckets from 1 µs to 1,024 µs.
  `handoffs_without_park` is in the stats response and in
  `pylon_vm_handoffs_without_park_total`.
- **`pylond` no longer writes `WAKING`** (§3). Two things depended on it, and
  now use `pylond`-owned or VMM-owned fields instead:
  - *Not picking a vCPU twice.* A vCPU is free for a core only while it is
    `PARKED` **and** its `assigned_pcpu` is unassigned. The engine sets
    `assigned_pcpu` when it assigns a core, and also when it starts a
    revocation for a recipient, to reserve it. It releases the reservation if
    the revocation is abandoned.
  - *Not reclaiming a core whose vCPU hasn't woken yet.* The engine records
    the vCPU's `yield_count` at assignment and reclaims the core only when the
    vCPU is `PARKED` with a higher count. The VMM increments `yield_count`
    before publishing `PARKED`.
- **The VMM initializes every slot's `assigned_pcpu` to `PCPU_UNASSIGNED`.**
  It used to leave the zeroed value, CPU 0, which the reservation check would
  read as taken.

### 4.6 Guest Side Requirements (`pylon/guest/`, Track 5)

The userspace `pylon-guest-agent` can't meet these requirements. It can't run
on the target vCPU at interrupt time, and it can't observe kernel critical
sections. It becomes **telemetry-only**: it keeps writing `runnable_threads`,
`head_wait_tsc`, and the heartbeat, and stops writing `state` and
`revoke_requested` (fixes P4).

A Linux kernel module `pv-pylon` (`pylon/guest/linux/`) must:
1. Bind to the `pylon-kick` PCI device, map the shared page using the GPA from
   BAR0, and check `PYLON_MAGIC` and `PYLON_ABI_VERSION == 2`.
2. Allocate `num_vcpus` MSI-X vectors with managed affinity.
3. In the kick handler on vCPU *i*: if `revoke_requested == 0`, return
   (spurious). Otherwise mark a per-CPU *park pending* flag and request a
   reschedule.
4. At the next safe point (preemption enabled, not in IRQ context, no
   spinlocks held), if *park pending*: set the `stranded_work_bitmap` bit for
   *i* when the local runqueue is non-empty, write `state = PARKING`, and write
   `PARK_DOORBELL`. Execution resumes there after wake.
5. Keep `in_critical_section` up to date, at minimum for paravirt spinlock
   slow paths and IRQ entry/exit. Exact hook points depend on guest kernel
   version (Q3).
6. Make other vCPUs treat a parked vCPU as preempted (`vcpu_is_preempted`).
   This matters because KVM sets `KVM_VCPU_PREEMPTED` in steal-time only for
   involuntary preemption, not for a userspace-blocked vCPU (Q2).

An Asterinas `ostd` integration follows the same contract. It is out of scope
for the first cut.

*As implemented (S5):* `pylon/guest/linux/pv_pylon.c`, with the page layout
mirrored in `pv_pylon_abi.h` (static assertions pin the same offsets as
`pylon-abi`). It covers items 1–4. Items 5 and 6 are still open.

- **Binding (item 1).** The driver checks the BAR0 magic and ABI version, maps
  the page from the GPA in BAR0 with `memremap(MEMREMAP_WB)`, and checks the
  page's magic, version and `num_vcpus` against BAR0. One device per guest.
- **Vectors (item 2): not managed.** `min(num_vcpus, nr_cpu_ids)` MSI-X
  vectors. Linux CPU *i* is assumed to be vCPU *i*. Managed affinity was
  dropped: Linux 7.0 spread vector 0 to CPU 1 of a two-CPU guest. Instead, a
  CPU-hotplug callback pins vector *i* to CPU *i* whenever CPU *i* comes
  online, and `IRQF_NOBALANCING` keeps irqbalance and userspace off it.
- **Kick handler (item 3).** If `revoke_requested == 0`, the kick is
  counted as spurious and ignored. Otherwise the handler sets vCPU *i*'s
  *park pending* flag and wakes CPU *i*'s park thread. A kick can land on
  another CPU in two cases:
  - The vector's CPU is offline, so the kernel moved the vector away.
  - On x86 without interrupt remapping, an affinity change takes effect at
    the vector's next interrupt, so the first kick after binding still lands
    on the old CPU.

  Either way the handler forwards the kick to CPU *i*'s thread with an IPI.
  It doesn't drop it.
- **Safe point (item 4).** Each CPU has a `SCHED_FIFO` kthread
  (`smpboot`, `pylon_park/<cpu>`). When it runs, the interrupted task was
  preempted with preemption enabled and no spinlock held.
  - The thread re-checks `revoke_requested`.
  - If another task is runnable (`!single_task_running()`), it sets its
    `stranded_work_bitmap` bit.
  - With preemption disabled, it writes `state = PARKING` (Release) and then
    `PARK_DOORBELL`.
  - After the wake, it clears the stranded bit.
- **`/dev/pylon`.** The module exposes the page as a misc device that can be
  mapped. The guest agent prefers it to `/dev/mem`. On x86, PAT refuses the
  agent's uncached `/dev/mem` mapping while the kernel holds a write-back
  one; `/dev/pylon` maps the page write-back too. Spec 07 §4.4's long-term
  plan wants that write-back mapping on aarch64 as well, but it hasn't been
  run there yet.
- **Counters.** `/sys/bus/pci/devices/<bdf>/stats` has `kicks`,
  `spurious`, `forwarded`, `parks` and `stale`. `stale` counts park-thread
  wakeups that found the revocation already gone.
- **Kernels and license.**
  - Builds warning-free (`W=1 -Werror`, checkpatch clean) against Ubuntu
    6.8 and 7.0 headers. `vm_flags_set` needs Linux 6.3 or later.
  - Licensed `GPL-2.0-only OR BSD-3-Clause`, so it can use GPL-only kernel
    symbols (`smpboot`, `irq_set_affinity`) and stay compatible with the
    repository's BSD-3-Clause.
- **Not done:**
  - Item 5, `in_critical_section`. The paravirt spinlock slow path and
    IRQ entry/exit have no module hooks.
  - Item 6, `vcpu_is_preempted`. `pv_ops` is read-only after init.

  Both need guest kernel changes (Q3). Until then, `pylond` doesn't see
  guest critical sections. It does no worse than before: it only ever sees
  0.

---

## 5. Upstream Touch Points

To match the isolation rules in `spec/00-architecture.md §2`:

| File | Change |
| :--- | :--- |
| `vmm/src/pylon.rs` | Decision table (§4.4). The kick `EventFd`s and the registration stay (§4.2). |
| `vmm/src/pylon_kick.rs` *(new)* | `PylonKickDevice`: `PciDevice` + `BusDevice` impl, MSI-X config, BAR0 registers. It sits beside `pylon.rs` so `pylon.rs` stays focused on shared-page and park state. |
| `vmm/src/device_manager.rs` | One `add_pylon_kick_device()` call when `--pylon` is set, modelled on `add_pvpanic_device()`. |
| `pci/src/msix.rs` | Forward `enable()` and `disable()` through `MaybeMutInterruptSourceGroup`. Upstream's wrapper falls back to the trait's no-op defaults, so disabling MSI-X never removed a mutable group's irqfds. |
| `vmm/src/cpu.rs` | Remove the `VmExit::Reset` hook. The existing post-exit hook stays. |
| `pylon/pylon-abi` | `PYLON_ABI_VERSION = 2`; doc comments for the §3 ownership table; BAR0 offset constants. |
| `pylon/pylond` | §4.5; record `peer_cred()` PID; `tgkill`. |
| `pylon/pylon-guest-agent` | Telemetry-only (§4.6). |

**Registration ordering:** `PylonVmmState::new` registers with `pylond` during
config parsing, before any devices exist. That stays: the `EventFd`s it sends
are the VMM's own, and the device installs them later (§4.2). Until the guest
driver enables MSI-X, a kick waits in its `EventFd`. That is harmless: the
forced path (§4.4) still applies.

---

## 6. Architecture & Backend Considerations

- **x86_64 / KVM:** primary target.
- **MSHV:** the device and MSI-X path go through the generic interrupt
  manager. We still need to verify that the MSHV backend maps an interrupted
  run (`EINTR`) to `VmExit::Ignore` the same way KVM does (Q4). Until then,
  `--pylon` should be rejected with a clear error on MSHV, so it doesn't
  silently lose the forced path.
- **aarch64:** the device, MMIO doorbell, and MSI-X (GICv3 ITS) are
  architecture-neutral. Blockers are `pylond`'s TSC usage and the
  `tsc`-denominated ABI fields, which need a counter abstraction (`CNTVCT_EL0`).
  The fixed shared-page GPA also has to be checked against
  `arch/src/aarch64/layout.rs` (`0xFEDF_0000` falls inside
  `MEM_32BIT_RESERVED` there). Out of scope here.

---

## 7. Lifecycle

- **Snapshot/restore and live migration:** reject them when `--pylon` is
  active, with an explicit error, until the shared page and `pylond`
  registration have a defined migration story. *Implemented (S4):* the
  `pylon-kick` device's snapshot fails with "VMs with --pylon can't be
  snapshotted or migrated".
- **vCPU hotplug:** `num_vcpus` is fixed at `max_vcpus`, so the MSI-X table
  size doesn't change. A hot-removed vCPU must be woken and must not be
  revoked again. `pylond` must skip slots with `host_tid == 0` when choosing
  donors and recipients (not verified against the current engine).
- **`pylond` restart:** the VMM keeps running. Parked vCPUs stay parked until a
  new `pylond` re-registers the VM, so the VMM needs a reconnect path (Q5).
- **Shutdown:** unchanged (`shutdown_all` wakes every parked thread).

---

## 8. Implementation Steps

Each step is a single reviewable commit series and leaves the tree working.

| Step | Scope | Fixes | Verification |
| :--- | :--- | :--- | :--- |
| **S1** | VMM: remove the `Reset` hook; §4.4 decision table (consent + deadline). | P2, P3 | Unit tests for every row of the §4.4 table; existing `vmm --features kvm --lib pylon` tests. |
| **S2** | `pylond`: stop writing `state`; `peer_cred` PID; `tgkill` on expiry; `ForceRevoking` substate; latency metric. | P5, P6 | `pylond` unit tests; `e2e_pylon` with a real KVM VM (no guest driver) shows forced parks with `state == PARKED` written by the VMM. |
| **S3** | `pylon-guest-agent` telemetry-only; ABI v2 bump. | P4 | `cargo test -p pylon-guest-agent -p pylon-abi`. |
| **S4** | `pylon-kick` device; the VMM's kick `EventFd`s become its MSI-X notifiers (§4.2). | P1 | Device unit tests (BAR0 decode); KVM test that writes a notifier and observes the interrupt in a guest. **Done (2026-10-31):** `cargo test -p vmm --features kvm --lib pylon_kick` covers the registers, config space, MSI-X table and a real-KVM check that a kick waits while its vector is disabled or masked and reaches KVM's irqfd once enabled. A Linux 7.0 guest booted with `--pylon` enumerated `1af4:10f0`, read the BAR0 registers, and, after enabling vector 0 by hand, logged `No irq handler for 0.208` for the kick that a stand-in `pylond` wrote. |
| **S5** | `pv-pylon` Linux kmod (`pylon/guest/linux/`). | — | Guest integration test: cooperative parks with `forced_preempt_count == 0` under a spinlock-heavy workload. **Built (2026-10-31), §4.6 items 1–4; items 5–6 open.**<br>**Stand-in `pylond`:** 4-vCPU Ubuntu 6.8 and 7.0 guests, every CPU running two spinners and a `/proc` reader. A stand-in revoked each vCPU 200 times, with a deadline that never expires so only a cooperative park counts. On both kernels, all 800 revocations parked cooperatively, with `forced_preempt_count == 0`. The VMM cleared `revoke_requested` each time, and the stranded-work bit was set while parked and cleared after. A kick with no revocation pending was ignored. Taking a CPU offline and back, and unloading the module, worked with no kernel warnings. Kick to `PARKED` was about 20 µs at the median, as seen by a Python poller, which includes Python's own overhead (S6 will measure it properly).<br>**Real `pylond`:** two VMs, `initial_active_vcpus=1`. Kicks reached the guests and some revocations parked cooperatively. Neither the 8 µs grace nor a 200 µs test build gave a clean result: `pylond` stopped revoking after boot because of a separate issue that also happens without `pv-pylon` (spec 05 K7). The real-`pylond` check is still to do. |
| **S6** | End-to-end latency: kick→`PARKED`, revoke→recipient running. | — | p99 within the 8 µs grace window on reference hardware. |

S1–S3 don't depend on S4/S5 and can land first. After S3, every park is a
forced park, which is honest about the current state, and the metrics show it.

---

## 9. Follow-ups (Not in This Spec)

- Move the shared page into a `pylon-kick` BAR and drop the fixed GPA
  `0xFEDF_0000`. That address lies between the IOAPIC (`0xFEC0_0000`) and the
  LAPIC (`0xFEE0_0000`), but `arch/src/x86_64/layout.rs` doesn't declare it, so
  a future upstream device could land on it. Since K4 the page is no longer
  guest RAM: it is absent from the E820 map, like the IOAPIC and LAPIC.
- aarch64 `pylond` (counter abstraction). Now specified in
  `spec/07-aarch64-support.md`.

---

## 10. Open Questions

- **Q1:** Which PCI vendor/device ID can the project use for `pylon-kick`?
  **Provisional (2026-10-31):** `0x1AF4:0x10F0` from QEMU's experimental range
  (§4.2), which unblocks S4. **Open:** request a PCI-SIG vendor ID for CMKL,
  and give `pylon-kick` a device ID under it before Pylon ships.
- **Q2:** IPIs and TLB shootdowns aimed at a parked vCPU. With the thread
  blocked in userspace, the interrupt stays pending in the in-kernel LAPIC, and
  a sender doing `smp_call_function_many(wait=1)` spins until `pylond` wakes the
  target. Options: (a) the guest marks parked vCPUs preempted so PV TLB flush
  defers instead of IPIing; (b) the guest sets the target's
  `stranded_work_bitmap` bit when it sends to a parked vCPU, so `pylond` wakes
  it within one tick; (c) the VMM watches for pending interrupts on parked
  vCPUs. This needs prototyping. The same issue already affects vCPUs that are
  initially parked (`vcpu_id >= initial_active_vcpus`).
- **Q3:** The minimum guest kernel version, and the exact hook points for
  `in_critical_section`. `pv-pylon` builds against Linux 6.3 and later
  (`vm_flags_set`). The hook points are still open, and they need guest
  kernel changes, not just the module (§4.6, items 5 and 6).
- **Q4:** Whether the MSHV backend turns signal-interrupted runs into
  `VmExit::Ignore`.
- **Q5:** The VMM reconnect protocol after a `pylond` restart.
- **Q6:** Whether `FORCE_HANDOFF_US = 50` is right, or whether it should be
  per-VM policy (`VmPolicyUpdate`).
