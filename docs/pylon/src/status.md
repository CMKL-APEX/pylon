# Status and roadmap

This page says what works in Pylon today, what doesn't, and what is
planned. It reflects `main` as of 2026-10-29. The detailed tracker is
[`spec/05-implementation-status.md`](https://github.com/CMKL-APEX/pylon/blob/main/spec/05-implementation-status.md).

## What works

| Area | Status |
|---|---|
| Shared page | Created per VM, mapped into the guest at a fixed address, and shared with `pylond`. Layout checked at compile time. |
| `--pylon` option | Parsed and validated. VMs boot with only `initial_active_vcpus` running. |
| Registration | The VMM registers with `pylond` at boot, passing the shared page and per-vCPU `eventfd`s. |
| Waking parked vCPUs | `pylond` wakes a parked vCPU when its VM has demand and a managed CPU is idle. |
| Taking back idle cores | When a vCPU parks on its own, `pylond` reclaims its core. |
| Forced revocation | When a donor misses its 8 µs deadline, `pylond` signals its vCPU thread, the VMM parks it, and the core moves. |
| Arbitration | Demand evaluation, urgency ordering and donor selection run every tick (see [The arbitration engine](developer-guide/arbitration.md)). |
| Runtime policy | `min_cores`, `max_cores` and `priority_weight` can be changed over the control socket. |
| Metering | Core-microseconds, cooperative yields, forced preemptions, handoffs without park and revocation-to-park latency per VM, over the socket and as Prometheus metrics. |
| `scx_pylon` | Attaches to a `sched_ext` kernel and runs each managed CPU's assigned vCPU thread. `pylond` attaches it automatically when the kernel supports it. |

## What doesn't work yet

**Cooperative revocation, within the grace period.** Taking a core
from a VM that is still running works. The parts for doing it
cooperatively now exist:
- the `pylon-kick` device interrupts the donor vCPU;
- the `pv-pylon` guest module parks it at a safe point.

Neither is fast enough yet:

- **Too slow for the grace period.** Kick to park takes about 20 µs on
  the hosts tested so far (measured coarsely), more than `pylond`'s
  8 µs grace period. Many revocations still end in a forced park.
- **No module, no cooperation.** A guest without `pv-pylon` never
  cooperates.
- **A vCPU can stay parked indefinitely.** Nothing yet wakes a parked
  vCPU that the guest still has work or IPIs for. When a VM gets fewer
  cores than it has managed vCPUs for a long time, the guest can report
  soft lockups (spec 06 Q2).

The host side is in place:
- The VMM parks a vCPU only when the guest asks to (by writing
  `PARKING`) or when the revocation's deadline has passed. It no longer
  parks a vCPU that may hold a guest lock.
- When a deadline passes, `pylond` signals the donor's thread so the VMM
  parks it, and moves the core once it has.

Expect `forced_preemptions` to grow, and `handoffs_without_park` to stay
at zero. The design is in
[Enhancements](developer-guide/enhancements.md) (spec 06, tracks 4 and
5).

**Other gaps** (tracked as K1–K6 in the status tracker's
[known issues](https://github.com/CMKL-APEX/pylon/blob/main/spec/05-implementation-status.md#known-issues-other)):

- **Scale to zero.** A VM that holds no cores is woken when a timer it
  armed before its vCPUs parked comes due: the VMM reads each vCPU's
  timer as it parks it. On x86_64 that is the TSC-deadline timer, which
  needs the in-kernel local APIC (the default with KVM) and a guest
  using TSC-deadline mode (Linux's default when the CPU offers it). On
  aarch64 it is the guest's EL1 virtual timer; a guest using the
  physical timer publishes none. Arriving packets don't wake
  such a VM yet: nothing on the host feeds the receive doorbell.
- **Unwritten telemetry.** Nothing writes `in_critical_section` or
  `stranded_work_bitmap` yet, so donor selection can't avoid lock
  holders, and wakeups can't prefer vCPUs with stranded work.
- **Counter scaling.** `pylond` converts the guest's counter values
  with the offset the VMM measures, assuming the guest counter runs at
  the host's rate. The offset is measured to about a microsecond. A VM restored with a different TSC rate would break that, but
  restore doesn't support Pylon yet anyway.
- **`min_cores` while idle.** A VM that reports no runnable threads
  wants zero cores, whatever its `min_cores`.
- **Guest side.** There is no guest kernel driver yet. The userspace
  agent estimates demand from `/proc` and can't report critical
  sections.
- **The shared page's address.** The page is no longer reported to the
  guest as RAM, but `0xFEDF_0000` is still a fixed address that Cloud
  Hypervisor's layout doesn't reserve. Moving the page into a PCI BAR
  (see the roadmap) removes it. We haven't yet booted a guest to check
  its memory map since the change.
- **Lifecycle.** Snapshot and live migration are refused with
  `--pylon`, since they don't account for Pylon state. vCPU hotplug and a `pylond` restart aren't handled: the
  VMM registers once, at boot, and doesn't reconnect.

## Roadmap

| Track | Work | State |
|---|---|---|
| 4 | Fix the VMM's park decision, make `pylond` force a clean park on deadline, add the `pylon-kick` device that delivers revocations as interrupts, measure handoff latency. | S1–S4 done ([spec 06](https://github.com/CMKL-APEX/pylon/blob/main/spec/06-guest-kick-path.md)); latency (S6) not measured |
| 5 | `pv-pylon` Linux guest kernel module: report critical sections, park at safe points. | Parking at safe points, the stranded-work bit and `/dev/pylon` are built. Critical sections and `vcpu_is_preempted` need guest kernel changes |
| — | Move the shared page from the fixed address into a PCI BAR. | Follow-up in spec 06 §9 |
| — | Run an aarch64 guest with Pylon end to end (the host side is done: [spec 07](https://github.com/CMKL-APEX/pylon/blob/main/spec/07-aarch64-support.md)). | In progress |
| — | Snapshot, restore and migration support. | Not designed; refused with `--pylon` until then |

[Enhancements](developer-guide/enhancements.md) explains the track 4
and 5 design.
