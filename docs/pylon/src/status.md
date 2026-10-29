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
| Arbitration | Demand evaluation, urgency ordering and donor selection run every tick (see [The arbitration engine](developer-guide/arbitration.md)). |
| Runtime policy | `min_cores`, `max_cores` and `priority_weight` can be changed over the control socket. |
| Metering | Core-microseconds, cooperative yields and forced preemptions per VM, over the socket and as Prometheus metrics. |
| `scx_pylon` | Attaches to a `sched_ext` kernel and runs each managed CPU's assigned vCPU thread. `pylond` attaches it automatically when the kernel supports it. |

## What doesn't work yet

**Cooperative revocation.** Taking a core from a VM that is still
running doesn't work end to end. These are the gaps:

- The per-vCPU "kick" that should interrupt the donor vCPU isn't
  connected to a guest interrupt, so the guest isn't told about a
  revocation.
- The VMM's park check doesn't wait for the guest to leave a critical
  section, so a vCPU can be parked while it holds a guest lock.
- `pylon-guest-agent` and `pylond` can mark a vCPU as parked while it
  is still running.

Until this is fixed, **don't count on revocations being cooperative**,
and expect `forced_preemptions` to be the normal outcome when cores are
contended. The design that fixes it is in
[Enhancements](developer-guide/enhancements.md) (spec 06, tracks 4
and 5).

**Other gaps** (tracked as K1–K6 in the status tracker's
[known issues](https://github.com/CMKL-APEX/pylon/blob/main/spec/05-implementation-status.md#known-issues-other)):

- **Scale to zero.** The arbiter can wake a VM that holds no cores when
  its next timer is due or packets arrive, but nothing writes either
  signal yet: no guest component sets `next_timer_tsc`, and nothing on
  the host feeds the receive doorbell. A VM whose vCPUs are all parked
  can't run the guest agent either, so it isn't woken.
- **Unwritten telemetry.** Nothing writes `in_critical_section` or
  `stranded_work_bitmap` yet, so donor selection can't avoid lock
  holders, and wakeups can't prefer vCPUs with stranded work.
- **Time bases.** The VMM writes a fixed `tsc_khz` of 3,000,000 (3 GHz)
  into the header, whatever the host's real TSC rate, so `pylond`'s
  grace period and metering are only right on a 3 GHz TSC. The guest
  agent writes `head_wait_tsc` from the *guest's* TSC, while `pylond`
  compares it with the *host's*. Under KVM the two differ by the VM's
  TSC offset, so the wait time `pylond` computes is skewed by that
  offset.
- **`min_cores` while idle.** A VM that reports no runnable threads
  wants zero cores, whatever its `min_cores`.
- **Guest side.** There is no guest kernel driver yet. The userspace
  agent estimates demand from `/proc` and can't report critical
  sections.
- **The shared page in the guest memory map.** The VMM adds the page to
  guest memory, so on x86_64 the guest's E820 table lists it as an
  8 KiB RAM range at `0xFEDF_0000`, overlapping the reserved range
  around it. We haven't checked how each boot path (direct kernel boot,
  firmware) and guest resolve that overlap. Moving the page into a PCI
  BAR (see the roadmap) removes the question.
- **Lifecycle.** Snapshot, restore and live migration don't account for
  Pylon state. vCPU hotplug and a `pylond` restart aren't handled: the
  VMM registers once, at boot, and doesn't reconnect. `pylond` isn't
  told when a VM exits, so it keeps the registration, and possibly the
  managed CPUs it gave that VM, until it restarts.
- **Architectures and backends.** `pylond` and `scx_pylon` are
  x86_64-only (they use the TSC and `TPAUSE`). Pylon has only been run
  on KVM.

## Roadmap

| Track | Work | State |
|---|---|---|
| 4 | Fix the VMM's park decision, make `pylond` force a clean park on deadline, add the `pylon-kick` device that delivers revocations as interrupts, measure handoff latency. | Designed ([spec 06](https://github.com/CMKL-APEX/pylon/blob/main/spec/06-guest-kick-path.md)); not started |
| 5 | `pv-pylon` Linux guest kernel module: report critical sections, park at safe points. | Designed (spec 06 §4.6); not started |
| — | Move the shared page from the fixed address into a PCI BAR. | Follow-up in spec 06 §9 |
| — | aarch64 `pylond` (a counter abstraction instead of the TSC). | Not designed |
| — | Snapshot, restore and migration support. | Not designed; to be rejected explicitly until then |

[Enhancements](developer-guide/enhancements.md) explains the track 4
and 5 design.
