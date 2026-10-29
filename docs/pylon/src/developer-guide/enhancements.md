# Enhancements

This page covers the planned changes to Pylon: first the work to make
revocation cooperative, which is the most important, then smaller and
later items. The authority is
[`spec/06-guest-kick-path.md`](https://github.com/CMKL-APEX/pylon/blob/main/spec/06-guest-kick-path.md)
(a draft under review). Progress is tracked in
[`spec/05-implementation-status.md`](https://github.com/CMKL-APEX/pylon/blob/main/spec/05-implementation-status.md)
as tracks 4 and 5.

## Problems in the current revocation path

The handoff in [Architecture](architecture.md#flow-handing-a-core-to-a-vm)
expects the donor vCPU to be interrupted, finish any critical section,
and park. Six problems prevent that today:

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

Every field gets one writer (spec 06 §3), and `PYLON_ABI_VERSION` goes
to 2 because the writer rules change, although the layout doesn't:

| Field | Writer |
|---|---|
| `state` | The guest, only `RUNNING → PARKING`, for its own vCPU. The VMM, everything else. **`pylond` never writes it.** |
| `revoke_requested` | `pylond` sets it; the VMM clears it when it parks the vCPU. |
| `forced_preempt_count` | Moves to the VMM, the only party that knows a forced park happened. |
| `yield_count`, `last_yield_tsc` | The VMM. |

### Kick delivery: the `pylon-kick` PCI device

A small virtual PCI device, created with `--pylon`, with one MSI-X
vector per vCPU (spec 06 §4.2). The guest driver allocates the vectors
with managed affinity, so vector *i* interrupts vCPU *i*. The VMM sends
`pylond` the device's MSI-X `eventfd`s in place of its own kick
`eventfd`s, so a write from `pylond` goes through the existing irqfd
path and arrives as an interrupt. BAR0 exposes the magic, ABI version,
vCPU count and the shared page's address, plus a park doorbell.

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

The `VmExit::Reset` hook is removed.

### Forced park: a signal from pylond

When a deadline passes, `pylond` sends `tgkill(vmm_pid, host_tid,
SIGRTMIN)` once (spec 06 §4.4–4.5). Cloud Hypervisor's vCPU threads
already ignore `SIGRTMIN` with a no-op handler, so `KVM_RUN` returns
`EINTR`, the KVM backend reports `VmExit::Ignore`, and the post-exit
hook force-parks. `pylond` learns the VMM's PID from `SO_PEERCRED` at
registration. It hands the CPU over only once the VMM has written
`PARKED`. As a last resort it hands it over 50 µs after the signal,
counted separately as `handoff_without_park`.

### The guest kernel module

`pv-pylon` (`pylon/guest/linux/`, spec 06 §4.6) binds to the device,
maps the page, allocates the vectors, keeps `in_critical_section` up to
date, and in its interrupt handler arranges to park at the next safe
point. It also has to make other vCPUs treat parked ones as preempted.
`pylon-guest-agent` becomes telemetry-only.

## Implementation steps

| Step | Scope | Fixes |
|---|---|---|
| S1 | VMM: the decision table; remove the `Reset` hook. | P2, P3 |
| S2 | `pylond`: stop writing `state`; `SO_PEERCRED`; `tgkill` on deadline; wait for `PARKED`; latency metric. | P5, P6 |
| S3 | Guest agent telemetry-only; ABI version 2. | P4 |
| S4 | The `pylon-kick` device and its MSI-X `eventfd`s; register with `pylond` after the device exists. | P1 |
| S5 | The `pv-pylon` kernel module. | — |
| S6 | End-to-end latency: kick to `PARKED`, and revocation to recipient running. | — |

S1–S3 don't depend on the device or the module and can land first.
After them, every park is a forced park. That's honest about where
things stand, and the metrics will show it.

## Open questions

These are from spec 06 §10:

- **Q1.** The PCI vendor and device ID for `pylon-kick`.
- **Q2.** Interrupts sent to a parked vCPU. A guest waiting for a TLB
  shootdown to finish spins until `pylond` wakes the target. Options:
  mark parked vCPUs preempted so the guest defers the flush, have the
  guest flag the target in `stranded_work_bitmap`, or have the VMM
  watch for pending interrupts on parked vCPUs. This also affects vCPUs
  parked at boot today.
- **Q3.** The minimum guest kernel version, and where to hook
  `in_critical_section`.
- **Q4.** Whether MSHV reports a signal-interrupted run the way KVM does.
- **Q5.** How a VMM reconnects after `pylond` restarts.
- **Q6.** Whether 50 µs is the right last-resort bound, or whether it
  should be per-VM policy.

## Other planned work

| Item | Notes |
|---|---|
| Move the shared page into the `pylon-kick` BAR | Removes the fixed address and the E820 overlap (spec 06 §9). |
| Write the real TSC rate | Query KVM's TSC frequency instead of writing 3 GHz into the header. |
| One time base | Decide whether the guest writes guest-TSC values and `pylond` converts them with the VM's TSC offset, or the ABI uses another clock. |
| Deregistration | Tell `pylond` when a VM exits, so it can reclaim the VM's CPUs. |
| Reconnect | Let a VMM register again after `pylond` restarts (Q5). |
| Lifecycle | Reject snapshot, restore and migration with `--pylon`, explicitly, until they're designed (spec 06 §7). |
| aarch64 | A counter abstraction (`CNTVCT_EL0`) in place of the TSC in `pylond`, and a check of the shared page's address against the aarch64 layout. |
| `scx_pylon` lookup | A thread-ID-to-CPU map, so `select_cpu` doesn't scan all 256 entries. |
| Socket permissions | Let `pylond` set its socket's owner and mode itself. |
