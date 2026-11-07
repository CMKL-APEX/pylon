# Specification 08: Parking vCPUs Inside KVM

> **Status:** `DRAFT`. Prototype first (§7). P0 is done (§7.1); P1 is done
> (§7.2); P2–P3 aren't.
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

Each signal costs the guest an exit and an entry. On a nested host, an
entry takes about as long as the 10 µs interval, so a guest with
interrupts off made no progress between signals: a P2 trace showed
storms of 0.35 s, about 21,600 entries at the same instruction (§7.3).
So the interval doubles with each signal, from `REVOKE_RESIGNAL_US` up to
`REVOKE_RESIGNAL_MAX_US` (1 ms). After a handoff without park, it grows
from the revocation's deadline.

aarch64 doesn't need this. `WFI` wakes on a pending interrupt even when
it is masked.

### 3.5 Halt polling

Before `kvm_vcpu_block` sleeps, KVM polls for up to `halt_poll_ns` (the
default grows to 200 µs). That burns host CPU on every park and every
spurious wake. The VMM sets a per-VM limit with `KVM_CAP_HALT_POLL`.

This also changes halt polling for the guest's own `HLT`, which then wakes
more slowly. The limit is a tunable. The prototype measures 0 and a
small value (§7).

### 3.6 Bounded park

§3.2 assumes something will interrupt a parked vCPU. That holds for a
vCPU the guest halted itself, but not for one parked where the guest
thinks it is running:
- **No timer armed.** A vCPU force-parked as it leaves idle, before its
  tick restarts, has no timer pending. A trace of a P2 run showed one
  that nothing woke for at least 47 s (§7.3).
- **No IPI.** The guest queues work on a CPU it thinks is running without
  sending an IPI, unless the new task should preempt the current one. The
  `pv-pylon` FIFO kthread is never preempted by ordinary tasks, so
  cooperative parks are exposed too.

So the VMM bounds a park inside KVM:
1. At a forced or cooperative park, it arms a per-vCPU POSIX timer that
   sends `kick_signal` to the vCPU thread after `PARK_IN_KVM_BOUND`
   (4 ms), and every 4 ms after that. Firing again covers a signal that
   lands just before `KVM_RUN` sleeps. A boot park has no bound: the
   vCPU has just come online and is idle.
2. At an exit with no core assigned, once the bound has passed, the VMM
   sets the vCPU runnable and stops the timer. The vCPU stays `PARKED`
   and runs as a woken vCPU (§4.3), on its VM's cores, until it halts
   again.
3. A core assignment stops the timer.

A busy guest's tick wakes its parked vCPUs about as often, so the bound
adds little for them. An idle vCPU that is parked gets one extra wake.

### 3.7 Guest park request after a forced park

The guest's park thread (`pv-pylon`, spec 06 §4.3) checks
`revoke_requested`, writes `PARKING`, then rings the park doorbell. If
the revocation's deadline passes between the kick and that thread, the
VMM force-parks the vCPU at its next exit, maybe in the middle of the
thread. With a park in userspace, the guest doesn't run again until a
core is assigned. Inside KVM, an interrupt lets it run (§3.2): the
thread writes `PARKING` over `PARKED` and rings the doorbell.

`pylond` assigns cores only to `PARKED` vCPUs, and only the VMM ends a
park. Left as it is, the vCPU drops out of arbitration and runs on the
host's terms. A P2 run left all three parkable vCPUs of an idle VM that
way (§7.3). So at an exit while parked inside KVM, the VMM turns
`PARKING` back into `PARKED` before it looks for a core assignment. The
vCPU was parked already, so the request is done and no park is counted.

## 4. Component Changes

### 4.1 VMM (`vmm/src/pylon.rs`, `vmm/src/cpu.rs`)

- Park as in §3.1, and handle `EINTR` as in §3.3. The run loop already
  returns `VmExit::Ignore` for `EINTR` and runs the post-exit hook.
- The interrupt check of §3.4, x86_64 only.
- The park bound of §3.6.
- Restoring `PARKED` over a late guest park request (§3.7).
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

**How `pylond` does it (P2).** The engine reads each VM's `woken` mask
every sweep, keeping only vCPUs that are still `PARKED` with no core
assigned. It records when each bit was set, by VM slot and vCPU.

- **No cores:** a VM that owns no core gets a zero-core wakeup once one
  of its vCPUs has been woken for `WOKEN_DEMAND_US`. A due timer doesn't
  give one: KVM fires it, and the vCPU shows up woken (§4.4).
  Until then its woken vCPUs run on the shared vCPU queue, on CPUs other
  VMs may own. An idle guest's vCPUs wake for ticks and IPIs and halt
  within microseconds; a core for each such wake cost a forced preemption
  of another VM, which took the core back as soon as the guest halted.
  With no grace period, a busy and an idle VM traded a core about 250
  times a second (§7.3).
- **Long wakes are demand:** a vCPU woken for longer than
  `WOKEN_DEMAND_US` (4 ms) adds one core to its VM's desired cores, up to
  `max_cores`. It shows demand that `runnable_threads` misses: work in
  the guest kernel, such as interrupt handling, and work since the
  agent's last report. 4 ms covers a few 1 ms turns and the guest's
  tick, so a vCPU that wakes for an IPI or a timer and halts again stays
  shared. The longest wake sets the
  VM's urgency, as `head_wait_tsc` does, weighted by its priority.
- **A core given to a woken vCPU stays wanted until the agent reports.**
  Once it has the core, the vCPU no longer counts as woken, and
  `runnable_threads` doesn't show its work until the guest agent's next
  report. So its VM's desired cores don't drop below the cores it has or
  is getting for vCPUs that were woken when picked, until
  `guest_heartbeat` moves or `WOKEN_GRANT_HOLD_US` (50 ms, the agent's
  default interval) passes. Then the usual demand applies. Without this,
  a VM that a zero-core wakeup gave a core wanted none in the next
  sweep, and the donor took it back as over its demand within 0.3 ms,
  before the vCPU ran on it. A bursty VM traded a core with a busy one
  every `WOKEN_DEMAND_US` while its burst lasted (§7.3). Holding the
  core until the vCPU parked doesn't work: an idle vCPU with a core
  halts inside KVM and parks only when its core is revoked, so an idle
  VM kept a core for good. The other rules still apply: the donor can
  take the core back for its fair share or its urgency.
- **Which vCPU gets the core:** a vCPU in `stranded_work_bitmap` first,
  then the longest-woken vCPU, then one with a due timer, then the first
  parked vCPU. Stranded tasks can't run until their vCPU has a core,
  and the guest counts them as runnable, so its VM keeps asking for
  cores. A woken vCPU already runs, so its assignment needs no wake-up:
  the signal of §3.3 interrupts its `KVM_RUN`, and the VMM writes
  `RUNNING`.
- **Metering:** each VM counts the time its vCPUs spent woken, split by
  whether the VM owned a core at the time (`woken_shared_usec`) or not
  (`woken_unowned_usec`). Shared time is already inside the VM's
  allocation. Unowned time ran on other VMs' CPUs or unmanaged ones, and
  the zero-core wakeup keeps it short.

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
- **`park_timer_deadline_tsc` only orders vCPUs.** KVM fires the timer
  itself, so a due timer no longer wakes a VM with no core, and neither
  does the guest's `next_timer_tsc`. `pylond` still uses the deadline to
  pick which vCPU of a VM gets a core first. The deadline was also
  stale: the VMM records it at the park and clears it only when the vCPU
  gets a core, while the guest re-arms its timer unseen. So once it
  passed, a VM with no core looked due again as soon as it lost a core,
  and traded one with a busy VM about ten times a second (§7.3).
- **The wake `EventFd`s drop out of registration** for KVM VMs.

## 5. Guest Impact (Unmodified Linux)

| Event | Today | With this spec |
|---|---|---|
| IPI or TLB shootdown to a parked vCPU | Sender spins until `pylond` assigns a core: no bound, soft lockups seen | KVM wakes the target; it runs on a shared CPU of its own VM. Expected wait: a few µs to tens of µs (to measure) |
| Guest timer on a parked vCPU | VMM publishes it; `pylond` wakes a zero-core VM | KVM fires it; the vCPU runs on a shared CPU |
| Guest checks `vcpu_is_preempted` | Never true for a parked vCPU | True while a shared vCPU waits its turn |
| Steal time | None counted while parked | Counted while runnable and waiting |
| Busy vCPU parked mid-work | Makes no progress | Wakes on its tick (every 1–4 ms), or after 4 ms if no timer is armed (§3.6), and makes progress on a shared CPU |
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

### 7.2 P1 result (2026-11-01)

**Implementation** (`pylon/park-in-kvm-p1`): §4.3, with `scx_pylon`
required.

**Run:**
- *Host:* x86_64, Ubuntu `7.0.0-42`, 64 vCPUs and no SMT. The host is
  itself a KVM guest (nested virtualization).
- *VMs and `pylond`:* the §1 setup, two 4-vCPU Ubuntu 26.04 guests with
  `initial_active_vcpus=1` and `PYLON_PARK_IN_KVM=1`, on 4 managed CPUs
  under `pylond --enable-scx`.
- *Guest load:* a busy loop pinned to every guest CPU, watched for 5
  minutes.

**First runs: the stall watchdog ejected `scx_pylon` within a minute.**
- *What happened:* a woken vCPU stayed runnable on its VM's queue for
  more than 30 s while its VM owned CPUs. Both guests logged soft
  lockups, and the host logged a hung task and an RCU stall.
- *Cause:* `dispatch` gave a woken vCPU its turn only when `prev` was the
  CPU's owner. A per-CPU kworker woke on every tick and ran between
  turns, so `prev` was never the owner and the owner kept the CPU.
- *Fix:* `running` records whether the owner or a woken vCPU ran last on
  each CPU, and `dispatch` gives the next turn by that.

**With the fix:**

| | Result |
|---|---|
| `scx_pylon` | attached for the whole run |
| Soft lockups, RCU stalls | none in either VM; none on the host |
| Woken vCPUs | ran only on their own VM's CPUs, about half a CPU each next to the owner |
| Steal time | about 150 s per guest on each of the two sharing CPUs |

**What it answers:**
- **V4, partly:** steal time is counted while a woken vCPU waits its
  turn. `KVM_VCPU_PREEMPTED` wasn't checked directly.
- **V6:** `sched_ext` works with KVM's wake-up of a halted vCPU on this
  kernel. The host has no SMT, so the core-scheduling issue of #15 wasn't
  tested.
- **The 14 s gap, since fixed:** a parked vCPU twice stayed runnable for
  about 14 s without running, while its `woken` bit was clear. See below.

**The gap (2026-11-02).**
- *What happened:* a timer interrupt preempted the parked vCPU while its
  migration was disabled, so `enqueue` put it on its CPU's local queue,
  behind the owner, a busy vCPU of the same VM. A `sched_ext` dump showed
  it there, and a pinned kworker waiting the same way on another owned
  CPU.
- *Cause:* when a task's slice ends, the kernel picks the next task, then
  puts the old one back through `enqueue`. For the owner, `enqueue`
  kicked its CPU to preempt. The kick lands after the switch, so it took
  the CPU from the task just picked. A task on the local queue got the
  CPU each time the owner's slice ended, every 20 ms, and lost it at
  once. The watchdog never fired, because the task was picked each time.
- *Fix:* no preempting kick for a task that the CPU itself puts back.
  This exposed two cases the kick had hidden: `dispatch` runs while the
  owner, or a woken vCPU whose turn ended, is still `prev` and not yet on
  its queue. It now counts that `prev` as waiting, and keeps it when
  nothing else of its VM waits, instead of giving the CPU to any vCPU on
  the vCPU queue.
- *Turns* now end with the slice, which the tick rounds up to about 1 ms,
  not at each kick: a sharing CPU switches about 1,000 times a second,
  down from 20,000 to 50,000.

**Runs (2026-11-02):** a 16-vCPU nested host (AMD EPYC, no SMT), otherwise
the setup above, with steady or port-I/O guest load:

| | Before the fix | With the fix |
|---|---|---|
| Runs | about 1 h 40 min; 70 min of it with Ubuntu guests | 15 min, Ubuntu guests |
| Runnable vCPU without progress for 3 s | none | none |
| Owner halted for most of a second, after boot | 1 to 10 in each 10-minute Ubuntu run | none |
| Host task pinned to an owned CPU, against an unmanaged CPU | 1 to 45 % | 50 to 85 % |
| Owner and woken vCPU on a shared CPU | about half each | about half each |

The gap itself didn't recur on this host, before or after the fix. The
pinned task shows the same cause every time. The `scx_pylon` live test
`test_pinned_task_shares_cpu_with_busy_owner` checks it.

### 7.3 Stalls in parked guests (2026-11-05)

**Setup:**
- *Host:* the 16-vCPU nested host of §7.2.
- *VMs:* two 4-vCPU Ubuntu guests with the fixed guest agent, which no
  longer counts itself as runnable.
- *Load:* VM 1 runs a busy loop on every CPU. VM 2 is either idle, or
  bursty: every 7 s, its CPUs 1 to 3 spin for 100 ms.
- *`pylond`:* 3 managed CPUs, `min_cores=0`.
- *Runs:* 180 s each. Lockups and RCU stalls are counted in the guests'
  serial logs.

**Before the fixes:**

| Run | Guest soft lockups | Guest RCU stalls |
|---|---|---|
| P1, VM 2 idle | 0 | 0 |
| P1, VM 2 bursty | 0 | 2 |
| P2, VM 2 idle | 2 | 0 |
| P2, VM 2 bursty | 6 | 4 |

**A traced P2 bursty run showed two causes:**
- *Signal storms (§3.4).*
  - A donor with interrupts off isn't parked, and `pylond` signalled it
    every 10 µs.
  - Each signal costs an exit and an entry, about 10 µs on this host,
    so the guest stayed at the same instruction.
  - The worst storm lasted 0.35 s, with about 21,600 entries.
- *Parks nothing ended (§3.6).*
  - A vCPU force-parked as it left idle had no timer armed.
  - Nothing woke it for at least 47 s, and its guest logged an RCU
    stall.
- In the same trace, a timer interrupt reached a vCPU halted by a
  cooperative park, but KVM didn't enter the guest. That is unexplained.
  One suspect is an x86 interrupt shadow (just after `sti`), which the
  `RFLAGS.IF` check of §3.4 doesn't see. Later runs found no park in a
  shadow (below).

**With the bound of §3.6 and the backoff of §3.4:**

| Run | Guest soft lockups | Guest RCU stalls |
|---|---|---|
| P1, VM 2 idle | 0 | 0 |
| P1, VM 2 bursty (2 runs) | 0 | 0 |
| P2, VM 2 idle | 3 | 0 |
| P2, VM 2 bursty (2 runs) | 0 | 0 |

**The P2 idle run hit a gap in `scx_pylon`:**
- *What happened:* a vCPU of VM 2 stayed runnable on the vCPU queue for
  30 s, during VM 2's boot. The watchdog ejected `scx_pylon`.
- *Why no CPU ran it:*
  - `pylond` made that vCPU the owner of CPU 2 while it already waited
    on the vCPU queue. `enqueue` doesn't run again for a queued thread,
    so it never reached CPU 2's owner queue.
  - CPU 2 gave its turns to VM 2's woken vCPUs instead, because its
    owner wasn't queued there.
  - Every other CPU skipped the vCPU, because it owned CPU 2.
- *Fix:*
  - `dispatch` also looks for a CPU's owner on the vCPU queue and on its
    VM's queue.
  - A CPU that skips a thread owning an idle CPU kicks that CPU.
  - When `rescue_stranded_vcpus` moves vCPUs to the vCPU queue, it kicks
    an idle CPU.
- *Test:* the live test `test_owner_given_its_cpu_while_queued_runs`
  gives a CPU to a thread while it is queued, ten times. Without the
  fix, the owner didn't run at all in 2 of the 10 cycles. With it, the
  test passed three times out of three.

**With the `scx_pylon` fix too,** no run had a lockup or an RCU stall,
and `scx_pylon` stayed loaded. P2 churned, though: 13,000 to 25,000
forced preemptions per run, against a few hundred for P1. Sampling each
vCPU's slot every 3 s showed two causes:
- *P2 woke a VM with no core for any woken vCPU.* An idle guest's
  vCPUs wake for ticks and IPIs and halt again within microseconds.
  Each wake took a core from VM 1 by a forced preemption, and VM 1 took
  it back once VM 2 halted: about 250 trades a second. P2 now waits for
  a long wake, as for a VM that owns a core (§4.2 on the P2 branch).
- *vCPUs stuck in `PARKING` (§3.7).* The trading stopped only once all
  three parkable vCPUs of VM 2 sat in `PARKING` with no core, out of
  `pylond`'s reach. A guest park thread had written `PARKING` over a
  forced park.

**With both fixes:**

| Run | Guest soft lockups | Guest RCU stalls | VM 2 forced preemptions |
|---|---|---|---|
| P1, VM 2 idle | 0 | 0 | 562 |
| P1, VM 2 bursty (2 runs) | 0 | 0 | 1,077 and 785 |
| P2, VM 2 idle | 0 | 0 | 875 |
| P2, VM 2 bursty (2 runs) | 0 | 0 | 2,106 and 3,186 |

No sample found a vCPU in `PARKING`. VM 2's metered core time fell to
5 to 11 s per run, about what its bursts need: 3 vCPUs spinning 100 ms
every 7 s is 7.7 s over 180 s. Earlier runs metered 70 to 160 s, mostly
cores it held while idle.

**Most of the remaining preemptions were stale timers.** Logging every
revocation in a P2 bursty run showed 1,550 of 3,726 were VM 2 taking a
core for a due park timer, and 1,820 were VM 1 taking it back once VM 2
wanted none. P1 had the same pattern: 842 and 925 of 1,890. Only about
120 per run were the two VMs contending for a core. A due timer no
longer wakes a VM that parks inside KVM (§4.4):

| Run | Forced preemptions (VM 2), before → after | vCPU run time, VM 1 / VM 2 (s), before → after |
|---|---|---|
| P1, VM 2 idle | 562 → 29 | 697 / 15 → 691 / 15 |
| P1, VM 2 bursty (2 runs) | 1,077 and 785 → 89 and 73 | 685 / 21 and 694 / 22 → 692 / 22 and 689 / 21 |
| P2, VM 2 idle | 875 → 97 | 690 / 16 → 695 / 16 |
| P2, VM 2 bursty (2 runs) | 2,106 and 3,186 → 335 and 386 | 690 / 22 and 688 / 20 → 692 / 21 and 693 / 22 |

Run time is the vCPU threads' time on a CPU, summed from the harness's
samples. VM 2's bursts spin for a fixed 100 ms of wall time, so equal
run time means they got as much done. No run had a lockup or an RCU
stall. P2's figures include its own changes (§4.2 on its branch).

**P2's extra preemptions were zero-core wakeups given back at once.**
Logging every revocation in a P2 bursty run on `main` showed 207 of 590
were VM 2 taking a core for a long wake, and most of the rest VM 1
taking it back as over VM 2's demand, 0.1 to 0.3 ms later. VM 2's vCPU
no longer counted as woken once it had the core, and the guest agent
hadn't reported the burst yet. During each burst the two traded a core
every `WOKEN_DEMAND_US`. A core given to a woken vCPU now stays wanted
until the agent reports again, or for 50 ms (§4.2):

| Run | Forced preemptions (VM 2), before → after | vCPU run time, VM 1 / VM 2 (s), before → after |
|---|---|---|
| P2, VM 2 idle | 97 → 57 | 695 / 16 → 696 / 15 |
| P2, VM 2 bursty (2 runs) | 335 and 386 → 85 and 121 | 692 / 21 and 693 / 22 → 687 / 20 and 687 / 23 |

P2 is now at P1's level. VM 2's metered core time was 3 to 11 s. A
first version held the core until its vCPU parked. An idle vCPU with a
core halts inside KVM and parks only when its core is revoked, so VM 2
kept a core for the whole run (173 s of core time) and VM 1 lost a
quarter of its run time.

**The hold costs VM 1 no measurable run time.** In the runs above VM 1
ran about 1% less with the hold. Four more bursty runs of each, on
`main` with the hold and with `WOKEN_GRANT_HOLD_US` set to 0, alternated:

| Hold | VM 1 vCPU run time (s) | VM 2 forced preemptions |
|---|---|---|
| 50 ms | 695.5, 687.4, 697.5, 688.2 (mean 692.2) | 113, 89, 115, 102 |
| Off | 696.0, 699.9, 691.3, 691.9 (mean 694.8) | 302, 332, 384, 336 |

The means differ by 0.4%, less than the spread within each set. No run
had a lockup or an RCU stall.

**The remaining trades follow VM 2's bursts.** Logging every revocation
in a bursty run with the hold showed 174 revocations, against 590
before:
- 47 were VM 2 taking a core for a long wake. It went back about 60 ms
  later (median), once the hold ended and the agent no longer reported
  the burst.
- 22 were VM 2 taking a core as under its fair share, after the agent
  reported runnable threads. Each went back at the agent's next report,
  50.3 ms later (median), once the burst was over. There were 57 before.
- Most of the rest were VM 1 taking those cores back.

So each core moves once per report period, and VM 2 keeps an idle core
for at most one period after a burst: about 1 s of core time per run.
Reporting more often would cut that, at a cost in guest CPU.

**No park happened in an interrupt shadow.** A temporary build read the
shadow state (`KVM_GET_VCPU_EVENTS`) at each park. Over 332 parks in a
bursty and an idle P2 run, 184 of them forced, none was inside a shadow.
That doesn't rule out a rare case, but nothing here needs a check for
it. Of the 320 parks that weren't boot parks, 300 ended by the bound of
§3.6 rather than by a wake.

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
