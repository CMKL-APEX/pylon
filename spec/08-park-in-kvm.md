# Specification 08: Parking vCPUs Inside KVM

> **Status:** `DRAFT`. Prototype first (§7). P0 is done (§7.1); P1–P3 aren't.
> Tracked as **Track 7** in `spec/05-implementation-status.md`. It answers
> spec 06 Q2 for unmodified guests.

## 1. Motivation

Today a parked vCPU's thread blocks in the VMM, on its wake `EventFd`
(`PylonVmmState::park_vcpu`), outside `KVM_RUN`. To the guest the vCPU is
online but never runs. Nothing wakes it until `pylond` gives it a core.

The guest still treats that CPU as one that will answer:

- **IPIs wait without bound.** `smp_call_function_many(wait=1)` and TLB
  shootdowns spin until every target answers. KVM keeps the interrupt
  pending in the in-kernel LAPIC, but no one runs the vCPU.
- **The guest can't tell.** KVM sets `KVM_VCPU_PREEMPTED` in steal time only
  when the vCPU thread is preempted while runnable. A thread blocked in
  userspace isn't, so `vcpu_is_preempted` and the paravirtual TLB flush
  don't skip it.
- **Pinned work and RCU stall** on the parked CPU.

**Observed (2026-10-31, after the K7 fix in spec 05):**

- *Setup:* two 4-vCPU Ubuntu 7.0 guests, `initial_active_vcpus=1`, on 4
  managed CPUs with `pylond --no-scx`. The guests pinned work to every CPU.
- *What happened:* they settled at 2 cores each, so each VM's third managed
  vCPU stayed parked. Both guests logged soft lockups and RCU stalls within
  30 s.

Spec 06 Q2 lists three fixes:

- (a) the guest marks parked vCPUs as preempted, which needs guest kernel
  changes;
- (b) the guest flags the target in `stranded_work_bitmap`, which also needs
  a guest kernel hook;
- (c) the VMM watches parked vCPUs for pending interrupts, polling from
  the host.

This spec proposes a fourth: park the vCPU **inside KVM**, so that KVM wakes
it when an interrupt arrives, as it does for a vCPU halted by the guest. It
needs no guest changes and no polling.

## 2. Goals & Non-Goals

**Goals**

- An unmodified Linux guest never waits without bound on a parked vCPU.
- Stock paravirtual features keep working on parked vCPUs: steal time,
  `KVM_VCPU_PREEMPTED`, paravirtual TLB flush and paravirtual spinlocks.
- Waking a parked vCPU doesn't take CPU time from another VM.
- No host CPU spent polling for interrupts.

**Non-goals**

- MSHV. It has no equivalent of a halted vCPU state that I know of; it keeps
  today's park.
- `pylond --no-scx`. This design depends on `scx_pylon` (§4.3).
- Replacing `pv-pylon`. Its doorbell stays the cooperative path. It decides
  *when* the guest gives a core back; this spec changes *how* the vCPU waits.

## 3. Design

### 3.1 Park

`check_and_park_vcpu` (`vmm/src/pylon.rs`) keeps the decision table of spec
06 §4.4. Only the action changes. The steps up to `PARKED` stay the same:
clear `revoke_requested`, bump `yield_count`, and publish `PARKED`. Then,
instead of `park_vcpu()`:

1. Set the vCPU to KVM's halted state with `Vcpu::set_mp_state`, which the
   hypervisor crate already has for snapshots:
   - `KVM_MP_STATE_HALTED` on x86_64;
   - `KVM_MP_STATE_SUSPENDED` on aarch64, which behaves like `WFI`.
2. Return to the run loop, which calls `KVM_RUN` again. KVM sees a halted
   vCPU and sleeps the thread in `kvm_vcpu_block` (after halt polling, §3.5).

### 3.2 Wake by interrupt

When an interrupt becomes pending for the vCPU, KVM wakes the thread, makes
the vCPU runnable and enters the guest. This happens without returning to
userspace. The guest resumes at the instruction where it was parked. The
interrupt can be an IPI, the guest's own timer (which replaces spec 05 K1's
`park_timer_deadline_tsc`), or a device MSI.

The VMM and `pylond` don't see this. The thread is now running guest code
with no core assigned. `scx_pylon` has to stop it from running anywhere it
likes (§4.3).

### 3.3 Wake for a core assignment

The wake `EventFd` can't reach a thread blocked in `KVM_RUN`. Calling
`KVM_SET_MP_STATE` from another thread isn't possible either, because
`KVM_RUN` holds the vCPU mutex. Instead:

- **`pylond`** signals the thread with `tgkill` and the VMM's `kick_signal`.
  It does this today for forced parks.
- **KVM** wakes the blocked thread on the pending signal, and `KVM_RUN`
  returns `EINTR`.
- **The VMM** sees `assigned_pcpu` set. It sets the vCPU runnable
  (`KVM_MP_STATE_RUNNABLE`), writes `RUNNING`, and re-enters.

An `EINTR` with no assignment is spurious. The VMM then goes back to
§3.1 step 2.

### 3.4 Forced park with interrupts disabled (x86_64)

KVM wakes a halted x86 vCPU for an external interrupt only if the guest can
take it (`kvm_vcpu_has_events`, needs checking, §6 V1). A vCPU force-parked
inside an interrupts-off section would ignore IPIs, and the problem in §1
would come back for that window.

At a forced park, the VMM reads `RFLAGS.IF`. If it is clear, the VMM
doesn't park. It returns `Continue`, and `pylond` re-signals 10 µs later
(`REVOKE_RESIGNAL_US`), which is the existing behaviour for a signal that
lands at a bad moment.

aarch64 doesn't need this. `WFI` wakes on a pending interrupt even when
it is masked.

### 3.5 Halt polling

Before `kvm_vcpu_block` sleeps, KVM polls for up to `halt_poll_ns` (the
default grows to 200 µs). That burns host CPU on every park and every
spurious wake. The VMM sets a per-VM limit with `KVM_CAP_HALT_POLL`.

This also changes halt polling for the guest's own `HLT`, which then wakes
more slowly. The limit is a tunable. The prototype measures 0 and a
small value (§7).

## 4. Component Changes

### 4.1 VMM (`vmm/src/pylon.rs`, `vmm/src/cpu.rs`)

- Park as in §3.1, and handle `EINTR` as in §3.3. The run loop already
  returns `VmExit::Ignore` for `EINTR` and runs the post-exit hook.
- The interrupt check of §3.4, x86_64 only.
- `KVM_CAP_HALT_POLL` at VM creation when `--pylon` is set.
- Drop the wake `EventFd`s, or keep them only for MSHV.
- Snapshot and migration stay refused (spec 06 §7).

### 4.2 `pylond`

- Wake for assignment by signal, not the wake `EventFd`.
- Read the *woken* set that `scx_pylon` publishes (§4.3), the same way
  it reads `stranded_work_bitmap` today. A woken vCPU wants to run.
- Policy for a woken vCPU, starting simple:
  1. by default it shares a core its VM already owns (§4.3);
  2. if it stays runnable longer than a bound, it is treated as demand and
     gets a core of its own through the normal matching (phase 3).
- Meter shared time against the VM. It is already inside the VM's own
  allocation, so no other VM is charged.

### 4.3 `scx_pylon` (now required)

- When a parked vCPU's thread becomes runnable (`ops.runnable` /
  `ops.enqueue`) and it has no assigned CPU:
  - queue it in a per-VM dispatch queue;
  - set its bit in a woken map that `pylond` reads (an mmapable BPF array
    like `pylon_pcpu_owners`).
- **Sharing:** a CPU owned by the same VM dispatches from the VM's queue
  with a short slice (proposed 100 µs). It runs the woken vCPU and its
  own vCPU in turns. The preempted one is runnable but not running, so KVM
  gives it steal time and `KVM_VCPU_PREEMPTED`. The guest's paravirtual
  spinlocks and TLB flush then work as on any overcommitted host.
- A CPU never runs another VM's woken vCPU, which keeps the isolation of
  spec 03 and the SMT rules of #16.
- **When the VM has no cores** (zero-core state), the woken bit is demand,
  as `zero_core_wakeup` is today.

**How `scx_pylon` knows a vCPU is parked (P1).** It can't read the shared
pages, and the vCPU list it gets from `pylond` changes only once a second.
So:

- **VM slots:** `pylond` gives each registered VM a slot (0–63), the lowest
  free one.
- **Parked and woken bits:** a new mmapable array, `pylon_vm_vcpus`, holds
  two `u64` bitmasks per slot.
  - *parked:* written by the engine at the end of each sweep. A vCPU's bit
    is set while it is `PARKED` with no core assigned. Only VMs that
    registered with `wake_by_signal` get bits.
  - *woken:* set by `scx_pylon` when such a vCPU is enqueued, and cleared
    in `quiescent` or once it owns a CPU.
- **Thread lookup:** `pylon_vcpu_dom` maps each vCPU thread to its VMM's
  tgid, its VM's slot, and its vCPU index.
- **Owned CPUs:** the owner entry's flags carry the owning VM's slot plus
  one, in bits 16–31. `scx_pylon` finds a VM's CPUs by scanning the owner
  entries.
- **Queues:** each slot has a DSQ, `PYLON_VM_DSQ + slot`.
- **When the VM owns no CPU,** a woken vCPU goes on the shared vCPU queue
  and runs anywhere, as in P0. Each `dispatch` also checks one VM queue
  in turn, and moves vCPUs whose VM no longer owns a CPU there, before the
  stall watchdog fires.

### 4.4 ABI (version 3)

- **`state` loses a writer.** The VMM can't write `RUNNING` after an
  interrupt wake, because it never sees one. `state` keeps its spec 06 §3
  meaning only for parks and assignments. Whether a parked vCPU is running
  on a shared CPU comes from `scx_pylon`'s woken map.
- **`park_timer_deadline_tsc` may become unused.** KVM fires the timer
  itself. `pylond` still uses it to decide which vCPU of a zero-core VM to
  wake first. Keep it until the prototype shows whether that matters.
- **The wake `EventFd`s drop out of registration** for KVM VMs.

## 5. Guest Impact (Unmodified Linux)

| Event | Today | With this spec |
|---|---|---|
| IPI or TLB shootdown to a parked vCPU | Sender spins until `pylond` assigns a core: no bound, soft lockups seen | KVM wakes the target; it runs on a shared CPU of its own VM. Expected wait: a few µs to tens of µs (to measure) |
| Guest timer on a parked vCPU | VMM publishes it; `pylond` wakes a zero-core VM | KVM fires it; the vCPU runs on a shared CPU |
| Guest checks `vcpu_is_preempted` | Never true for a parked vCPU | True while a shared vCPU waits its turn |
| Steal time | None counted while parked | Counted while runnable and waiting |
| Busy vCPU parked mid-work | Makes no progress | Wakes on its tick (every 1–4 ms) and makes progress on a shared CPU |
| Other VMs | Unaffected | Unaffected: sharing stays within the VM |

The cost is concentrated in the parked VM. When its vCPUs have more work
than it has cores, it behaves like an ordinary overcommitted KVM guest.
That is the right failure mode.

## 6. Points to Verify in the Prototype

- **V1** x86 `kvm_vcpu_has_events`: does a halted vCPU with `IF = 0` wake on
  an external interrupt? If not, §3.4 is needed. Does it on aarch64 with
  `SUSPENDED`?
- **V2** Setting `HALTED`/`SUSPENDED` on a vCPU that wasn't executing `HLT`
  or `WFI`: after the wake, does the guest resume at the same instruction,
  with no side effect? Migration only restores vCPUs that really halted.
- **V3** Does `KVM_RUN` return `EINTR` promptly for a signal to a thread
  blocked in `kvm_vcpu_block`, including during halt polling?
- **V4** Is steal time and `KVM_VCPU_PREEMPTED` set for a woken vCPU that
  `scx_pylon` keeps runnable but doesn't run? (`kvm_sched_out` sets
  `vcpu->preempted` when the task is still on the run queue.)
- **V5** How often do parked vCPUs wake under real workloads (idle, busy,
  TLB-heavy)? This sets the cost of sharing.
- **V6** `sched_ext` interaction with KVM's wakeup of a halted vCPU,
  including the core-scheduling bug of #15 on Ubuntu 7.0.

## 7. Prototype Plan

On a `sched_ext` host. The aarch64 reference host of spec 07 qualifies; an
x86_64 `sched_ext` host is needed for V1 and §3.4.

| Step | Scope | Result |
|---|---|---|
| **P0** | VMM only, behind an environment variable: park as §3.1, wake by signal as §3.3, halt polling off. No `scx_pylon` change, so woken vCPUs run freely. | Answers V1–V3. The two-VM run of §1 no longer reports soft lockups. Count wakes per parked vCPU per second (V5). |
| **P1** | `scx_pylon` gating and per-VM sharing (§4.3), plus the woken map. | Answers V4 and V6. Woken vCPUs run only on their VM's CPUs. |
| **P2** | `pylond` reads the woken map; meters shared time; escalates long wakes. | The §1 workload runs with no stalls. Other VMs' core time is unchanged. |
| **P3** | Measurements: wake-to-run latency for IPIs to parked vCPUs; guest throughput against the current design; halt-polling settings. | Data to decide whether this replaces the current park. |

Only after P3: an ABI v3 spec update, and a decision on whether this design
replaces the park of spec 06 for KVM, or sits next to it.

### 7.1 P0 result (2026-10-31)

**Implementation** (`pylon/park-in-kvm-p0`):
- The VMM parks inside KVM when `PYLON_PARK_IN_KVM=1`. Halt polling is
  off.
- It registers with `wake_by_signal`. `pylond` then signals the vCPU on
  assignment, and again every 10 µs while the vCPU stays `PARKED`. A
  signal that lands just before `KVM_RUN` would otherwise be lost.
- The vCPU seccomp filter needed `KVM_SET_MP_STATE` and, on x86_64,
  `KVM_GET_REGS`.

**Run:**
- *Host:* x86_64, Ubuntu `7.0.0-42`.
- *VMs and `pylond`:* the §1 setup, two 4-vCPU guests with
  `initial_active_vcpus=1` on 4 managed CPUs under `pylond --no-scx`.
- *Guest load:* a busy loop pinned to every guest CPU, watched for 5
  minutes.
- *Also on the host:* an unrelated, unmanaged VM.

| | Park in the VMM | Park in KVM (P0) |
|---|---|---|
| Cores, VM 1 / VM 2 | 1 / 3 | 2 / 2 |
| Soft lockups, RCU stalls | VM 1: 16 and 2, then the guest stopped responding | none in either VM |
| Guests alive at the end | VM 2 only | both, load average about 4 |

**What it answers:**
- **V2:** the guests resumed after every park and ran their load to the
  end, with no errors in the guest log.
- **V3:** every core assignment ran its vCPU. No assigned vCPU stayed
  `PARKED`.
- **V5, partly:** a parked vCPU with a busy pinned task wakes on its next
  timer tick. It then never halts again, so it keeps running with no core
  of its own, on any CPU, until it is assigned one. The guests' load
  average of 4 on 2 cores each shows this. P1 has to bound it.
- **Not answered:**
  - V1 (§3.4 avoids the case, so the run never tested it);
  - wake rates for idle or TLB-heavy guests;
  - V4 and V6 (P1).

## 8. Alternatives Considered

**(c) VMM polling.**
- *How it works:* the VMM checks each parked vCPU's LAPIC (`KVM_GET_LAPIC`)
  for pending interrupts and asks `pylond` to wake it.
- *For it:* no `scx_pylon` dependency, and a small change.
- *Against it:* detection waits for the next poll, polling costs host CPU,
  and the guest's paravirtual features still don't see parked vCPUs.
- *Use:* a fallback for MSHV, or for `--no-scx`.

**(a) and (b), guest-side.** Both need guest kernel changes (spec 06 Q3).
(a) remains useful with this spec: marking parked vCPUs as preempted saves
the wake for TLB flushes, which is cheap here but not free.
