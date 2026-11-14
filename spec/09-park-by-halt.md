# Specification 09: Cooperative Park by Halt

> **Status:** `DRAFT`, on hold. H0 (§9) found that the guest takes
> 16–64 µs from the kick's injection to its park request, so a park by
> halt can't reach the 8 µs grace either; H1 waits until the guest's
> time is understood. It aims at #74: a cooperative park within
> `pylond`'s 8 µs grace period. It builds on parking inside KVM (spec
> 08) and needs `--pylon park=kvm` and `scx_pylon`.

## 1. Motivation

With `pv-pylon` loaded, a guest takes 80–140 µs on average from a
revocation request to `PARKED`, against `pylond`'s 8 µs grace, so every
revocation ends in a forced park (spec 06 S5, #74). A 200 µs grace
makes most parks cooperative but gains no throughput, so the grace
stays.

**Where the time goes (2026-11-13, #74).** Measured on the bare-metal
x86_64 host (Zen 2) with `bpftrace` on the VMM's vCPU threads:

- **The guest's side** (the kick interrupt, waking the `pv-pylon` FIFO
  thread, writing `PARKING`) was taken to be a few µs. H0 measured it
  later: 20–100 µs from the kick to the doorbell (§9).
- **The host's exit to the VMM is most of it.** A cooperative park ends
  with the doorbell's `KVM_EXIT_MMIO`, a forced one with a signal. Both
  then return from `KVM_RUN`, run the VMM's park decision, call
  `KVM_SET_MP_STATE` and enter `KVM_RUN` again. With the host's
  CPU-vulnerability mitigations on (an IBPB before each return to user
  space after a guest ran, return thunks, retpolines with STIBP, SSBD
  under seccomp), each kernel step on that path takes several µs:
  signal delivery 8–16 µs, `KVM_SET_MP_STATE` 2–8 µs,
  `kvm_arch_vcpu_put` 1–4 µs on every return.
- **Waking a vCPU for a core costs the same.** `pylond` signals the
  thread, `KVM_RUN` returns `EINTR`, and the VMM makes the vCPU runnable
  (spec 08 §3.3). That is most of the 100–126 µs median handoff (#53).

The aarch64 reference host parks a real vCPU in about 15 µs (#62), so
x86 hosts pay most of this.

A faster guest yield can't reach the grace on x86: the exit to the VMM
alone is longer. A cooperative park has to stay inside the kernel. (H0
found that this isn't enough either, §9.)

## 2. Goals & Non-Goals

**Goals**
- A cooperative park that doesn't leave KVM: the guest halts itself,
  and neither a doorbell exit nor a signal is involved.
- `pylond` sees the guest's consent and moves the core within the grace
  period.
- Waking a vCPU parked this way for a core doesn't leave KVM either.
- Guests and VMMs of `v0.0.8` keep working: the doorbell park stays.

**Non-Goals**
- **Forced parks.** A guest that doesn't answer is still parked by a
  signal and the VMM (spec 06 §4.4, spec 08 §3.1).
- **`park=vmm`.** A halt there doesn't block the thread outside the
  guest in a way the VMM sees, so parking in the VMM keeps the doorbell.
- **MSHV.** `park=kvm` is refused there already.
- **Host mitigation settings.** Measuring with them off is useful
  (§7 H0), but Pylon can't ask hosts to turn them off.

## 3. Design

Under `park=kvm`, a parked vCPU is a halted one (spec 08 §3.1), and a
vCPU that halts itself is blocked by KVM in `kvm_vcpu_block` without
leaving the kernel. The only things the VMM adds at a cooperative park
are bookkeeping: clear `revoke_requested`, count the yield and publish
`PARKED`. `pylond` can do those itself.

### 3.1 Park

`pv-pylon`'s park thread, on a revocation:

1. Sets its stranded-work bit if other tasks wait (unchanged).
2. Writes `PARKING` to its slot (release), as today.
3. If the page offers park by halt (§4.1), halts (`safe_halt()` on
   x86_64, `wfi()` on aarch64) instead of ringing the doorbell, and
   returns when the halt ends.

KVM blocks the thread: halt polling is already off for these VMs
(spec 08 §3.5), so it sleeps at once, and `scx_pylon` sees it go
quiescent.

### 3.2 `pylond` accepts the park

`pylond`'s Plane 2 already spins over its VMs. For a vCPU with a core,
it now also reads the guest slot's `state`:

- **`PARKING`**, with the VM's park by halt on (§4.1): `pylond` takes
  this as the park. It moves the host slot from `RUNNING` to `PARKED`
  with a compare-and-swap (§4.2), and only if that succeeds: clears
  `revoke_requested` in both slots, bumps `yield_count`, records the
  park as by halt (§4.2), writes `PARKED` to the guest slot, counts a
  cooperative yield, and hands the core over as after any park.
- **Otherwise** nothing changes: a revocation whose deadline passes is
  forced, as today.

`pylond` doesn't wait for the halt. The thread is at most a few
instructions from it, and once the core moves it is a parked vCPU
whose thread is still runnable: `scx_pylon` runs it as a woken vCPU of
its VM (spec 08 §4.3) until it halts. So consent alone moves the core,
and a guest that writes `PARKING` and doesn't halt gains nothing it
couldn't get by waking from a park (§5).

### 3.3 Wake by interrupt

As after any park inside KVM (spec 08 §3.2): an interrupt ends the
halt, the park thread returns, and the guest runs on as a woken vCPU,
still `PARKED`. Unlike a forced park, the guest's tick stays armed: the
park thread halted outside the idle loop, so the tick wakes the vCPU
within a jiffy. The VMM's bound of spec 08 §3.6 isn't needed.

### 3.4 Wake for a core assignment

A vCPU parked by halt never left `KVM_RUN`, so the VMM has nothing to
undo. `pylond` wakes it without a signal:

1. Assigns the core (`assigned_pcpu`, `scx_pylon`'s map), as today.
2. Moves the host slot from `PARKED` to `RUNNING` with a
   compare-and-swap, and writes `RUNNING` to the guest slot.
3. Writes the vCPU's kick `EventFd`. KVM delivers the MSI in the kernel
   (irqfd), which ends the halt.

The kick handler finds `revoke_requested` clear and returns; the park
thread's halt has ended, and the guest runs on its new core. A vCPU
woken earlier by an interrupt is already running as a woken vCPU, and
`scx_pylon` moves it to its core.

`pylond` doesn't write the vCPU's wake `EventFd` for such a vCPU. The
VMM reads that `EventFd` at exits of a vCPU it parked (spec 08 §4.4),
and a stale count would end its next park at once.

A vCPU the VMM parked (forced, at boot, or by doorbell) is still woken
by a signal (spec 08 §3.3): the host slot records which (§4.2).

### 3.5 What stays

- **Forced parks**, by signal and the VMM, with their back-off.
- **The doorbell park**, for guests whose page doesn't offer park by
  halt, for `park=vmm` and for a `pylond` that doesn't support it.
- **§3.7 of spec 08.** A park request after a forced park is still
  turned back into `PARKED` by the VMM at the next exit.

## 4. Component Changes

### 4.1 Feature flag

- **Shared page header.** A `features` word from the header's reserved
  bytes, with `PYLON_FEATURE_HALT_PARK` (bit 0). The VMM writes it before
  the guest boots, from `pylond`'s registration reply. The layout and
  `PYLON_ABI_VERSION` (3) stay: a `v0.0.8` guest reads 0 there and rings
  the doorbell.
- **Registration.** The reply gains `halt_park: bool`. `pylond` sets it
  for a VM that parks inside KVM when `scx_pylon` is attached. A VMM
  that gets no such field leaves the flag clear.
- **Option.** `--pylon halt_park=on|off`, default `on`, to turn it off
  for a VM while it is measured.

The guest can write the header, so `pylond` keeps its own copy of
whether the VM may park by halt and never reads the guest's.

### 4.2 Host page (version 4)

- **The host slot's `state` gains a writer.** `pylond` moves it from
  `RUNNING` to `PARKED` for a park by halt, and back for its wake. The
  VMM's forced and cooperative parks and its wake move it with a
  compare-and-swap too, so one park is counted once when the guest's
  `PARKING` is seen by both (a doorbell exit racing `pylond`'s sweep).
- **`park_kind`**, a new host slot byte: `VMM` or `HALT`, written by
  whoever published `PARKED`. `pylond` reads it to pick a signal or an
  interrupt for the wake (§3.4).
- **`yield_count`** is bumped by whoever wins the compare-and-swap.
- `PYLON_HOST_VERSION` becomes 4. The VMM and `pylond` must be updated
  together, as for every host page change.

### 4.3 `pylond`

- Plane 2: the `PARKING` check of §3.2, and the wake of §3.4.
- The engine's `PARKED` vCPUs are unchanged: a vCPU parked by halt is
  a recipient and a woken vCPU like any other.
- Metrics: cooperative yields by halt, apart from doorbell ones, and
  the request-to-park latency of each.

### 4.4 VMM

- Compare-and-swap for the host slot's `state` (§4.2), and `park_kind`.
- At an exit of a vCPU whose host slot is `PARKED` with `park_kind`
  `HALT`, the VMM does nothing: it didn't park the vCPU, and the vCPU is
  running as a woken one.
- The feature flag (§4.1).

### 4.5 `pv-pylon`

- Halt instead of the doorbell when the flag is set (§3.1).
- A stat for parks by halt, and one for kicks that end a halt (wakes).

### 4.6 `scx_pylon`

No change expected: the park thread's halt is a quiescent vCPU, and
the time before it is a woken one's.

## 5. Guest Trust

The guest can write `PARKING` at any time, and halt or not.

- **`PARKING` without a revocation** is a voluntary park, allowed today.
- **`PARKING` without a halt:** the core moves anyway, and the vCPU runs
  as a woken one, on its VM's cores, which `pylond` meters (spec 08
  §4.2). No more than a guest gets by staying awake after a
  wake.
- **`PARKING` on another vCPU's slot** parks that vCPU's core. The guest
  can already park its own vCPUs; this only lets it do so from another
  CPU.
- **A forged `features` word** doesn't matter: `pylond` uses its own
  copy. A guest that halts without the flag stays on its core until the
  deadline, and is then forced.

## 6. Points to Verify

- **V1. Guest-side latency.** From the kick `EventFd` write to `PARKING`
  in the guest, on the bare-metal x86_64 host, with and without AVIC.
  The design needs it well under 8 µs.
- **V2. The halt blocks.** With halt polling off, a guest `HLT` from the
  park thread blocks the vCPU thread in `kvm_vcpu_block` without an
  exit to the VMM (x86_64), and so does `WFI` (aarch64). On aarch64,
  KVM may not trap `WFI` when the vCPU thread is the only task on its
  CPU (`kvm-arm.wfi_trap_policy`); then the thread stays on the CPU
  until `scx_pylon` preempts it for the new owner.
- **V3. Wake latency.** From the kick `EventFd` write to the vCPU running
  on its new core, against the signal path's 40–60 µs.
- **V4. RCU and watchdogs.** The halted CPU isn't idle to the guest.
  The tick ends the halt within a jiffy, so RCU and the soft-lockup
  watchdog should see no change; check with two VMs under load, as in
  spec 08 §7.4.
- **V5. Races.** A doorbell exit (an older guest, or the flag turned off
  at run time) racing `pylond`'s sweep; a forced park whose signal lands
  as the guest writes `PARKING`; a wake by interrupt racing a wake for
  a core.

## 7. Prototype Plan

| Step | Content | Exit criteria |
|---|---|---|
| **H0** | Measure V1 with the current doorbell park (`bpftrace` on the kick `EventFd` write and the `PARKING` store). Optionally, the doorbell park with mitigations off, which needs a host reboot. | V1 is well under 8 µs; the cost of mitigations on the exit path is known. |
| **H1** | `pv-pylon` halts; `pylond` accepts `PARKING` (§3.2). Wake still by signal. | Request-to-park within 8 µs for most revocations in the two-VM run of #55; no stalls. |
| **H2** | Wake by interrupt (§3.4). | Handoff median (#53's measure) well under the current 100–126 µs. |
| **H3** | Host page version 4, feature flag, option, docs. | Throughput at least the forced-park baseline's, in the runs of spec 08 §7.4 and §7.5. |

H1 is worth keeping only if cooperative parks then beat forced ones:
spec 06 S5 found no throughput gain from cooperation at a 200 µs grace,
when it was slower than the forced path. At 8 µs it would be faster.

## 8. Alternatives Considered

- **Park from the kick interrupt handler**, without the FIFO thread.
  It cuts the guest's side, which isn't where the time goes (§1), and
  isn't a safe point for most guest code.
- **Wait for the halt before moving the core** (`scx_pylon` publishing
  that the thread went quiescent). Safer-looking but slower by a
  scheduler round trip, and not safer: a parked vCPU that runs is
  already confined to its VM's cores.
- **A hypercall to park.** It would need a KVM change; a halt already
  does what is needed.
- **Turning the host's mitigations off.** Not Pylon's call; useful only
  to measure (H0).

## 9. H0 Result (2026-11-14)

*Run:* the bare-metal x86_64 host (Zen 2, AVIC off, so KVM injects
interrupts on the vCPU's own thread). Two 4-vCPU Ubuntu 7.0 guests on 3
managed CPUs; VM 1 with `pv-pylon` and the `coop` load, VM 2 `burst`;
`pylond` with a 200 µs grace, so that the guest's park isn't cut short
by a forced one. `bpftrace` for 20 s, about 70 cooperative parks per
run, keyed by VM and vCPU:

- the kick: `pylond`'s `EventFd` write, which injects the MSI in
  `pylond`'s context (`kvm_arch_set_irq_inatomic`);
- the injection of that vector on the vCPU (`svm_inject_irq`);
- the guest's doorbell write (`kvm_io_bus_write` at BAR0 + 0x40), and
  the exit before it.

| Step | Typical | Range |
|---|---|---|
| Kick → injection (host: IPI, exit, injection) | 8–32 µs | 4–32 µs |
| Injection → doorbell exit (the guest) | 16–32 µs | 16–64 µs |
| Doorbell exit → KVM's MMIO write (host) | 8–16 µs | 8–32 µs |
| Kick → doorbell | 32–64 µs | 16–128 µs |

Between the injection and the doorbell, the vCPU usually takes no exit
but the doorbell's own (1–3 exits, 0–4 µs on the host in all), and its
thread never leaves its CPU. So the guest itself spends the 16–64 µs.
It doesn't change with:

- a guest load that only spins in user space (`steady`) instead of
  `coop`'s `/proc` reader, so it isn't kernel code holding off the
  park thread;
- `mitigations=off` on the guest's command line, so it isn't the
  guest's own mitigations.

Where those µs go is open. Ubuntu 7.0's kernel preempts lazily
(`PREEMPT_LAZY`), but a `SCHED_FIFO` wakeup still preempts at once.
The next step is tracing inside the guest (the kick handler, the park
thread's wakeup and run).

**H0's exit criterion fails.** A park by halt would remove the
doorbell's exit and the VMM's path, but `pylond` would still see
`PARKING` 20–60 µs after the kick, against the 8 µs grace. H1 waits
until the guest's side is understood and cut. Measuring the host's
mitigations (a reboot) is also deferred: they don't explain the guest's
time.
